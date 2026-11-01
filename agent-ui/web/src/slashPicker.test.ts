import { describe, expect, it } from "vitest";
import { EFFORT_FROM_SETTINGS, effortPicker, modelPicker, parseEffortReply, parseModelReply, settingsChangedWords } from "./slashPicker";
import type { ModelOption } from "./types";

// Verbatim probe text from CLI 2.1.283 under a test-account wrapper,
// so the parser is checked against actual command output.
const MODEL_REPLY =
  "Current model: `Haiku 4.5` (effort: high)\n" +
  "Usage: /model <name>. Available: sonnet, opus, haiku, fable, best, sonnet[1m], opus[1m], " +
  "fable[1m], opusplan, default, or a full model ID.";

const EFFORT_USAGE_REPLY = "Usage: /effort <low|medium|high|xhigh|max|auto>";
const EFFORT_ERROR_REPLY = "Invalid argument: bogus. Valid options are: low, medium, high, xhigh, max, auto";

describe("parseModelReply", () => {
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

describe("parseEffortReply", () => {
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

describe("the pickers from the session's own list", () => {
  const row = (value: string, over: Partial<ModelOption> = {}): ModelOption => ({
    value, resolvedModel: value, displayName: value.toUpperCase(), description: "", supportsEffort: true,
    supportedEffortLevels: ["low", "max"], supportsAutoMode: true, ...over,
  });

  it("keeps the CLI's order and marks the named row over default, which resolves to the same id", () => {
    const picker = modelPicker([row("default", { resolvedModel: "m" }), row("opus", { resolvedModel: "m" })], { model: "m", effort: "" });
    expect(picker?.options).toEqual(["default", "opus"]);
    expect(picker?.current).toBe("opus");
  });

  it("marks default when no other row runs the model, and nothing for a model off the list", () => {
    const models = [row("default", { resolvedModel: "m" }), row("other")];
    expect(modelPicker(models, { model: "m", effort: "" })?.current).toBe("default");
    expect(modelPicker(models, { model: "unlisted", effort: "" })?.current).toBeNull();
  });

  it("says on its row that a model cannot run auto", () => {
    expect(modelPicker([row("old", { supportsAutoMode: false })], { model: "", effort: "" })?.labels.old).toBe("OLD (no auto mode)");
  });

  it("names no row when the read of what the session runs did not answer, a row with no id included", () => {
    const models = [row("a", { resolvedModel: "" }), row("b", { resolvedModel: "", supportsEffort: false })];
    expect(modelPicker(models, { model: "", effort: "" })?.current).toBeNull();
    expect(effortPicker([row("b", { resolvedModel: "", supportsEffort: false }), row("a", { resolvedModel: "" })], { model: "", effort: "" })).not.toBeNull();
  });

  it("has no picker for an empty list", () => {
    expect(modelPicker([], { model: "", effort: "" })).toBeNull();
  });

  it("offers the running model's levels and the way back to the settings", () => {
    const picker = effortPicker([row("m", { supportedEffortLevels: ["low"] }), row("n")], { model: "m", effort: "low" });
    expect(picker?.options).toEqual(["low", EFFORT_FROM_SETTINGS]);
    expect(picker?.current).toBe("low");
  });

  it("offers every level when the list does not name the model", () => {
    const picker = effortPicker([row("a", { supportedEffortLevels: ["low"] }), row("b", { supportedEffortLevels: ["max"] })], { model: "z", effort: "" });
    expect(picker?.options).toEqual(["low", "max", EFFORT_FROM_SETTINGS]);
    expect(picker?.current).toBeNull();
  });

  it("has no effort picker for a model that takes no level", () => {
    expect(effortPicker([row("m", { supportsEffort: false })], { model: "m", effort: "" })).toBeNull();
  });
});

describe("what the band says of a change", () => {
  const applied = { model: "new", effort: "low" };

  it("names the next turn when none runs", () => {
    expect(settingsChangedWords("model", applied, "New", null)).toBe("model: new, from the next turn");
  });

  it("names the old model during a turn", () => {
    expect(settingsChangedWords("model", applied, "New", { model: "old" })).toBe("model: new; the running response finishes on old");
  });

  it("does not claim a change back to the model running", () => {
    expect(settingsChangedWords("model", applied, "New", { model: "new" })).toBe("model: new, for the rest of this turn");
  });

  it("uses the chosen row when the read said nothing", () => {
    expect(settingsChangedWords("model", { model: "", effort: "" }, "New", null)).toBe("model: New, from the next turn");
  });

  it("says when a level holds, and that a model takes none", () => {
    expect(settingsChangedWords("effort", applied, "low", { model: "old" })).toBe("effort: low, from the next request");
    expect(settingsChangedWords("effort", { model: "m", effort: "" }, "low", null)).toBe("effort: none on this model, from the next request");
  });

  it("does not call an unanswered read a model with no level", () => {
    expect(settingsChangedWords("effort", { model: "", effort: "" }, "low", null)).toBe("effort: low, from the next request");
  });
});
