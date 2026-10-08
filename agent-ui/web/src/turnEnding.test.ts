import { describe, expect, it } from "vitest";
import { latestTurnEnding, resetTimeText, turnEndingBandText, turnEndingText } from "./turnEnding";
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

describe("a Codex turn ending", () => {
  it("usage limit ending shows the reset time", () => {
    // 2030-03-20 12:34:56 UTC; the wording is local time, so the test builds the same text.
    const resetsAt = Date.UTC(2030, 2, 20, 12, 34, 56) / 1000;
    const at = new Date(resetsAt * 1000);
    const time = at.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
    const ending = e("limit_reached", { reason: "usage_limit", message: "You've hit your usage limit." });
    const text = turnEndingText(ending, resetsAt);
    expect(text).toMatch(/^stopped at the usage limit \(resets .*\d\d:\d\d\): You've hit your usage limit\.$/);
    expect(text).toContain(time);
    // A reset on another day names the day.
    expect(text).toMatch(/resets \w{3} \d{1,2} \w{3} \d\d:\d\d/);
    expect(turnEndingBandText(ending)).toBe("limit reached");
  });

  it("a usage limit with no known reset time says only that", () => {
    const ending = e("limit_reached", { reason: "usage_limit" });
    expect(turnEndingText(ending)).toBe("stopped at the usage limit");
    expect(turnEndingText(ending, null)).toBe("stopped at the usage limit");
  });

  it("today's reset names the time alone", () => {
    const now = new Date(2026, 9, 20, 9, 0, 0);
    const soon = new Date(2026, 9, 20, 14, 5, 0).getTime() / 1000;
    expect(resetTimeText(soon, now)).toBe("14:05");
  });

  it("a full context, a spending limit and a rate limit have their own words", () => {
    const full = e("limit_reached", { reason: "context_window_exceeded", message: "Codex's context is full" });
    expect(turnEndingText(full)).toBe("stopped: the context is full: Codex's context is full");
    expect(turnEndingBandText(full)).toBe("context full");
    const budget = e("limit_reached", { reason: "session_budget" });
    expect(turnEndingText(budget)).toBe("stopped at the spending limit");
    expect(turnEndingBandText(budget)).toBe("spending limit");
    const rate = e("limit_reached", { reason: "rate_limited" });
    expect(turnEndingText(rate)).toBe("rate limited");
    expect(turnEndingBandText(rate)).toBe("rate limited");
  });
});
