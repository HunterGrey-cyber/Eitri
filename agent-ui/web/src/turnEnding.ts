import type { AgentUiState, TurnEnding } from "./types";

type Wording = Pick<TurnEnding, "kind" | "reason" | "apiErrorStatus" | "message">;

/** Terminal reasons Claude Code itself counts as the context being full (its own classifier calls all three
 *  `context_limit`): the conversation no longer fits, and waiting changes nothing. Verdandi reports the first
 *  and the last as a limit and `prompt_too_long` as a failure, so they are worded by reason, not by kind.
 *  `context_window_exceeded` is Codex's word for the same thing. */
const CONTEXT_FULL = new Set(["blocking_limit", "prompt_too_long", "rapid_refill_breaker", "context_window_exceeded"]);

/** Terminal reasons for a spending limit: Claude Code's budget, and a Codex session's. */
const SPENDING_LIMIT = new Set(["budget_exhausted", "session_budget"]);

/** Terminal reasons for a turn a hook stopped -- a pause, not an error, in Claude Code's own terms. */
const HOOK_STOPPED = new Set(["hook_stopped", "stop_hook_prevented"]);

/** Other terminal reasons Claude Code does not count as errors. Verdandi reports every reason it does not map
 *  as a failure, so a turn that ended for one of these is said to have stopped, not to have failed. */
const NOT_AN_ERROR = new Set(["tool_deferred", "background_requested", "max_turns", "completed", "aborted_streaming", "aborted_tools"]);

/** When a usage window resets, as a person reads it: the time alone for today, with the day otherwise. `resetsAt`
 *  is Unix seconds, as the agent reported it. */
export function resetTimeText(resetsAt: number, now: Date = new Date()): string {
  const at = new Date(resetsAt * 1000);
  const time = at.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (at.toDateString() === now.toDateString()) return time;
  return `${at.toLocaleDateString("en-GB", { weekday: "short", day: "numeric", month: "short" })} ${time}`;
}

/** The row's one line, in plain words, for a turn that did not complete. The provider's own message follows
 *  after a colon when it said anything; it is shown as it came (Rust already trimmed and capped it), and
 *  nothing here reads a reset time out of it -- if the CLI wrote one into its text, it is in the text.
 *  `resetsAt` is the reset time the ending kept from the account's limit reports, when Codex or Claude Code
 *  gave one: a turn stopped by the usage limit says when it ends. `ranFor` is how long the
 *  turn ran, already worded (`formatDuration`), when Rust stamped it: ` after 12s` follows the headline. */
export function turnEndingText(ending: Wording, resetsAt: number | null = null, ranFor: string | null = null): string {
  const message = ending.message !== null && ending.message !== "" ? `: ${ending.message}` : "";
  // How long the turn ran, said before the provider's own words so those stay the row's last line.
  const after = ranFor === null ? "" : ` after ${ranFor}`;
  const tail = `${after}${message}`;
  const reason = ending.reason ?? "";
  if (reason === "no_result") return `the turn did not finish: Claude Code went idle without a result${after}`;
  switch (ending.kind) {
    case "interrupted":
      return `interrupted${after}`;
    case "lost":
      return `the turn did not finish: the session ended${after}`;
    case "limit_reached":
    case "failed":
      break;
  }
  if (CONTEXT_FULL.has(reason)) return `stopped: the context is full${tail}`;
  if (HOOK_STOPPED.has(reason)) return `stopped by a hook${tail}`;
  if (ending.kind === "limit_reached") {
    if (reason === "max_turns") return `stopped at the turn limit${tail}`;
    if (SPENDING_LIMIT.has(reason)) return `stopped at the spending limit${tail}`;
    if (reason === "usage_limit") {
      const when = resetsAt !== null && Number.isFinite(resetsAt) ? ` (resets ${resetTimeText(resetsAt)})` : "";
      return `stopped at the usage limit${when}${tail}`;
    }
    if (reason === "rate_limited") return `rate limited${tail}`;
    return `stopped at a limit${reason !== "" ? ` (${reason})` : ""}${tail}`;
  }
  // A 429 is where a usage or rate limit shows up: Verdandi reports it as an API error with that status.
  if (ending.apiErrorStatus === 429) return `rate or usage limit reached (HTTP 429)${tail}`;
  if (NOT_AN_ERROR.has(reason)) return `the turn stopped (${reason})${tail}`;
  const named = [
    reason !== "" && reason !== "api_error" ? reason : null,
    ending.apiErrorStatus !== null ? `HTTP ${ending.apiErrorStatus}` : null,
  ].filter((part): part is string => part !== null);
  return `the turn ended with an error${named.length > 0 ? ` (${named.join(", ")})` : ""}${tail}`;
}

/** The band's few words for the latest ending; the row has the detail. */
export function turnEndingBandText(ending: Wording): string {
  const reason = ending.reason ?? "";
  if (reason === "no_result") return "turn did not finish";
  switch (ending.kind) {
    case "interrupted":
      return "interrupted";
    case "lost":
      return "turn did not finish";
    case "limit_reached":
    case "failed":
      break;
  }
  if (CONTEXT_FULL.has(reason)) return "context full";
  if (HOOK_STOPPED.has(reason)) return "stopped by a hook";
  if (ending.kind === "limit_reached") {
    if (reason === "max_turns") return "turn limit";
    if (SPENDING_LIMIT.has(reason)) return "spending limit";
    if (reason === "rate_limited") return "rate limited";
    return "limit reached";
  }
  if (ending.apiErrorStatus === 429) return "rate limited";
  if (NOT_AN_ERROR.has(reason)) return "turn stopped";
  return "turn failed";
}

/** The ending the band shows: the latest one, while `lastTurnEnding` says the latest turn ended that way.
 *  Both folds append an ending as they set `lastTurnEnding`, so it is the collection's last entry. */
export function latestTurnEnding(state: Pick<AgentUiState, "turnEndings" | "lastTurnEnding">): TurnEnding | null {
  if (state.lastTurnEnding === null) return null;
  return state.turnEndings[state.turnEndings.length - 1] ?? null;
}
