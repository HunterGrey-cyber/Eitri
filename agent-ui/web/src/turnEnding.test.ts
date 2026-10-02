import { describe, expect, it } from "vitest";
import { latestTurnEnding, turnEndingBandText, turnEndingText } from "./turnEnding";
import type { TurnEnding } from "./types";

const e = (kind: TurnEnding["kind"], over: Partial<TurnEnding> = {}) => ({ kind, reason: null, apiErrorStatus: null, message: null, ...over });

describe("turnEndingText", () => {
  it("interrupted says only that, whatever the reason", () => {
    expect(turnEndingText(e("interrupted"))).toBe("interrupted");
    expect(turnEndingText(e("interrupted", { reason: "aborted_streaming" }))).toBe("interrupted");
  });
  it("an error names its reason and HTTP status and the provider's message when there are any", () => {
    expect(turnEndingText(e("failed"))).toBe("the turn ended with an error");
    expect(turnEndingText(e("failed", { reason: "api_error", apiErrorStatus: 500 }))).toBe("the turn ended with an error (HTTP 500)");
    expect(turnEndingText(e("failed", { message: "boom" }))).toBe("the turn ended with an error: boom");
    expect(turnEndingText(e("failed", { reason: "api_error", apiErrorStatus: 529, message: "overloaded" }))).toBe(
      "the turn ended with an error (HTTP 529): overloaded",
    );
    expect(turnEndingText(e("failed", { reason: "model_error" }))).toBe("the turn ended with an error (model_error)");
    expect(turnEndingText(e("failed", { reason: "model_error", apiErrorStatus: 500 }))).toBe("the turn ended with an error (model_error, HTTP 500)");
  });
  it("a 429 reads as a rate or usage limit", () => {
    expect(turnEndingText(e("failed", { apiErrorStatus: 429 }))).toBe("rate or usage limit reached (HTTP 429)");
    expect(turnEndingText(e("failed", { reason: "api_error", apiErrorStatus: 429, message: "slow down" }))).toBe(
      "rate or usage limit reached (HTTP 429): slow down",
    );
  });
  it("a full context says so, whichever kind Verdandi gave it", () => {
    expect(turnEndingText(e("limit_reached", { reason: "blocking_limit", message: "Prompt is too long" }))).toBe(
      "stopped: the context is full: Prompt is too long",
    );
    expect(turnEndingText(e("limit_reached", { reason: "rapid_refill_breaker" }))).toBe("stopped: the context is full");
    expect(turnEndingText(e("failed", { reason: "prompt_too_long" }))).toBe("stopped: the context is full");
  });
  it("a turn a hook stopped, or one that stopped for another reason that is not an error, is not called an error", () => {
    expect(turnEndingText(e("failed", { reason: "hook_stopped" }))).toBe("stopped by a hook");
    expect(turnEndingText(e("failed", { reason: "stop_hook_prevented", message: "blocked by policy" }))).toBe("stopped by a hook: blocked by policy");
    expect(turnEndingText(e("failed", { reason: "tool_deferred" }))).toBe("the turn stopped (tool_deferred)");
    expect(turnEndingText(e("failed", { reason: "background_requested" }))).toBe("the turn stopped (background_requested)");
  });
  it("the turn limit and the spending limit are named, and any other limit by its reason", () => {
    expect(turnEndingText(e("limit_reached", { reason: "max_turns" }))).toBe("stopped at the turn limit");
    expect(turnEndingText(e("limit_reached", { reason: "max_turns", message: "Reached maximum number of turns (3)" }))).toBe(
      "stopped at the turn limit: Reached maximum number of turns (3)",
    );
    expect(turnEndingText(e("limit_reached", { reason: "budget_exhausted" }))).toBe("stopped at the spending limit");
    expect(turnEndingText(e("limit_reached", { reason: "something_new" }))).toBe("stopped at a limit (something_new)");
    expect(turnEndingText(e("limit_reached"))).toBe("stopped at a limit");
  });
  it("lost says the session ended", () => {
    expect(turnEndingText(e("lost"))).toBe("the turn did not finish: the session ended");
  });
  it("a message's line breaks are kept as they came, and an empty one is none", () => {
    expect(turnEndingText(e("failed", { message: "line one\nline two" }))).toBe("the turn ended with an error: line one\nline two");
    expect(turnEndingText(e("failed", { message: "" }))).toBe("the turn ended with an error");
  });
});

describe("turnEndingBandText", () => {
  it("is a few words per way a turn ends", () => {
    expect(turnEndingBandText(e("interrupted"))).toBe("interrupted");
    expect(turnEndingBandText(e("lost"))).toBe("turn did not finish");
    expect(turnEndingBandText(e("failed"))).toBe("turn failed");
    expect(turnEndingBandText(e("failed", { apiErrorStatus: 429 }))).toBe("rate limited");
    expect(turnEndingBandText(e("failed", { reason: "prompt_too_long" }))).toBe("context full");
    expect(turnEndingBandText(e("limit_reached", { reason: "blocking_limit" }))).toBe("context full");
    expect(turnEndingBandText(e("failed", { reason: "hook_stopped" }))).toBe("stopped by a hook");
    expect(turnEndingBandText(e("failed", { reason: "tool_deferred" }))).toBe("turn stopped");
    expect(turnEndingBandText(e("limit_reached", { reason: "max_turns" }))).toBe("turn limit");
    expect(turnEndingBandText(e("limit_reached", { reason: "budget_exhausted" }))).toBe("spending limit");
    expect(turnEndingBandText(e("limit_reached"))).toBe("limit reached");
  });
});

describe("latestTurnEnding", () => {
  const ending = (seq: number, kind: TurnEnding["kind"]): TurnEnding => ({ seq, turnId: `t${seq}`, kind, reason: null, apiErrorStatus: null, message: null });
  it("is the last ending while the latest turn ended that way, and nothing once the next turn started", () => {
    const turnEndings = [ending(3, "failed"), ending(9, "interrupted")];
    expect(latestTurnEnding({ turnEndings, lastTurnEnding: "interrupted" })).toEqual(turnEndings[1]);
    expect(latestTurnEnding({ turnEndings, lastTurnEnding: null })).toBeNull();
    expect(latestTurnEnding({ turnEndings: [], lastTurnEnding: null })).toBeNull();
  });
});
