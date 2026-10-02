import type { AgentUiState, TurnEnding } from "./types";

type Wording = Pick<TurnEnding, "kind" | "reason" | "apiErrorStatus" | "message">;

/** Terminal reasons Claude Code itself counts as the context being full (its own classifier calls all three
 *  `context_limit`): the conversation no longer fits, and waiting changes nothing. Verdandi reports the first
 *  and the last as a limit and `prompt_too_long` as a failure, so they are worded by reason, not by kind. */
const CONTEXT_FULL = new Set(["blocking_limit", "prompt_too_long", "rapid_refill_breaker"]);

/** Terminal reasons for a turn a hook stopped -- a pause, not an error, in Claude Code's own terms. */
const HOOK_STOPPED = new Set(["hook_stopped", "stop_hook_prevented"]);

/** Other terminal reasons Claude Code does not count as errors. Verdandi reports every reason it does not map
 *  as a failure, so a turn that ended for one of these is said to have stopped, not to have failed. */
const NOT_AN_ERROR = new Set(["tool_deferred", "background_requested", "max_turns", "completed", "aborted_streaming", "aborted_tools"]);

/** The row's one line, in plain words, for a turn that did not complete. The provider's own message follows
 *  after a colon when it said anything; it is shown as it came (Rust already trimmed and capped it), and
 *  nothing here reads a reset time out of it -- if the CLI wrote one into its text, it is in the text. */
export function turnEndingText(ending: Wording): string {
  const message = ending.message !== null && ending.message !== "" ? `: ${ending.message}` : "";
  const reason = ending.reason ?? "";
  switch (ending.kind) {
    case "interrupted":
      return "interrupted";
    case "lost":
      return "the turn did not finish: the session ended";
    case "limit_reached":
    case "failed":
      break;
  }
  if (CONTEXT_FULL.has(reason)) return `stopped: the context is full${message}`;
  if (HOOK_STOPPED.has(reason)) return `stopped by a hook${message}`;
  if (ending.kind === "limit_reached") {
    if (reason === "max_turns") return `stopped at the turn limit${message}`;
    if (reason === "budget_exhausted") return `stopped at the spending limit${message}`;
    return `stopped at a limit${reason !== "" ? ` (${reason})` : ""}${message}`;
  }
  // A 429 is where a usage or rate limit shows up: Verdandi reports it as an API error with that status.
  if (ending.apiErrorStatus === 429) return `rate or usage limit reached (HTTP 429)${message}`;
  if (NOT_AN_ERROR.has(reason)) return `the turn stopped (${reason})${message}`;
  const named = [
    reason !== "" && reason !== "api_error" ? reason : null,
    ending.apiErrorStatus !== null ? `HTTP ${ending.apiErrorStatus}` : null,
  ].filter((part): part is string => part !== null);
  return `the turn ended with an error${named.length > 0 ? ` (${named.join(", ")})` : ""}${message}`;
}

/** The band's few words for the latest ending; the row has the detail. */
export function turnEndingBandText(ending: Wording): string {
  const reason = ending.reason ?? "";
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
    if (reason === "budget_exhausted") return "spending limit";
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
