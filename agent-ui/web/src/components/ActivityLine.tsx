import { useEffect, useState } from "react";
import { elapsedWords, liveTasks, tasksLine } from "../band";
import { taskWakeText } from "../timeline";
import type { AgentUiState, TurnClock } from "../types";
import type { PanelMode } from "../keymap";
import { cliPhaseText, phaseOf } from "../turnPhase";
import { TurnActivity } from "./TurnActivity";
import { revealedText } from "../revealHidden";

type Props = {
  state: AgentUiState;
  onOpenTasks?: () => void;
  /** How long the current turn has been running, tracked in `App.tsx` (`TurnClock`) rather than in
   *  `AgentUiState` -- see that type's own doc comment. `null` before any turn has started, and
   *  optional/defaulted for callers (existing tests, mainly) that render no in-flight indicator at
   *  all. Only read while a turn is actually working; see `TurnActivity`. */
  turnClock?: TurnClock | null;
  /** From the provider's advertised capabilities, never from the backend's name: whether this line
   *  draws its Stop button. */
  canInterrupt: boolean;
  onInterrupt: () => void;
  /** How many items are queued behind the running turn. Shown beside the
   *  interrupt key, never with `pendingTool`, which takes over the same slot instead. */
  queued?: number;
  /** The tool name of the oldest pending permission card, or `null` when none waits. While it is
   *  set, this line names the card rather than the queue count -- a card already means the queue
   *  cannot flush, so the two are never both worth reading. */
  pendingTool?: string | null;
  /** Which mode the panel is in: the question line offers `i` in BROWSE. Defaults to `"browse"`. */
  mode?: PanelMode;
  /** How card mode is entered from here, said beside a waiting card: `"on-card"` (BROWSE on the card's row, or the
   *  tool row it gates: Enter or Alt-n), `"elsewhere"` (Alt-n), or `"held"` while card mode holds a card, when the line
   *  only names the card: j scrolls it and Enter answers it there. Defaults to `"elsewhere"`. */
  cardEntry?: "on-card" | "elsewhere" | "held";
  /** A `!` line of the person's is running. A turn of the agent's has this line first. */
  shellRunning?: boolean;
};

/** One mono line above the composer while a turn runs: motion, elapsed time and interrupt. With no turn
 *  running it shows idle background work in the same slot. The interrupt words are the Stop button itself,
 *  keeping the mouse route available. */
export function ActivityLine({ state, turnClock = null, canInterrupt, onInterrupt, queued = 0, pendingTool = null, shellRunning = false, mode = "browse", cardEntry = "elsewhere", onOpenTasks }: Props) {
  // A terminal status wins over `activeTurnId`, so a dead session cannot read as working.
  // The reducer clears it on `session_unavailable`/`session_closed`; keep this guard for other endings.
  const working = state.status.kind === "running" && state.activeTurnId !== null;
  const [now, setNow] = useState(Date.now);
  const [compactingSince, setCompactingSince] = useState(Date.now);
  useEffect(() => { if (state.compacting) setCompactingSince(Date.now()); }, [state.compacting, state.activeTurnId, state.sessionId]);
  const tasks = liveTasks(state.backgroundTasks.rows);
  const workflowLive = tasks.some((task) => task.kind === "workflow");
  const ticking = state.status.kind === "running" && (working ? state.compacting : workflowLive);
  useEffect(() => {
    if (!ticking) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [ticking]);
  if (state.questions.some((q) => q.state.kind !== "settled")) return (
    <div className="activity-line activity-question">⚑ Claude has a question{mode === "browse" ? " — i to answer" : ""}</div>
  );
  // Only a person's choice on the dock settles a plan: the line names where that is, never a card key.
  if ((state.plans ?? []).some((p) => p.state.kind !== "settled")) return (
    <div className="activity-line activity-question">⚑ plan needs review{mode === "browse" ? " — i to review" : ""}</div>
  );
  if (!working && shellRunning) return (
    <div className="activity-line" data-nav-stop="status">
      <span className="turn-activity">Running…</span>
      <button type="button" className="stop" onClick={onInterrupt}>ctrl+c interrupt</button>
    </div>
  );
  if (!working) return state.status.kind === "running" && tasks.length > 0 ? (
    <div className="activity-line tasks-line" data-nav-stop="status">
      <button type="button" onClick={onOpenTasks}>{tasksLine(tasks, now)}</button>
    </div>
  ) : null;
  const special = cliPhaseText(state);
  const limit = special?.startsWith("Usage limit reached") === true;
  const start = state.turnStarts.find((r) => r.turnId === state.activeTurnId);
  const cause = start?.cause;
  const causeText = cause?.kind === "task" ? taskWakeText(cause)
    : cause?.kind === "peer" ? `Message from @${cause.peerName ?? "unknown"}: ${cause.text ?? ""}`
    : cause?.kind === "scheduled" ? cause.scheduleReason ?? cause.prompt ?? "Scheduled wake-up"
    : "Claude Code started this turn";
  const phase = phaseOf(state);
  // The star is the line's one moving thing, and it moves only while the agent is working: a card
  // waiting, or a phase that is the user's to end, holds it still (`index.css`'s `turn-breath`).
  const moving = pendingTool === null && phase.kind !== "blocked" && !limit;
  return (
    <div className="activity-line" data-nav-stop="status">
      <span className="activity-glyph" aria-hidden="true" data-moving={moving ? "true" : undefined}>
        ✻
      </span>
      {" "}
      {/* A phase word beside the card cuts off the approval keys at 520px, so the card owns the line. */}
      {pendingTool === null && (special !== null ? (
        <span className="turn-activity">{revealedText(special)}{state.compacting && !state.apiRetry ? ` ${elapsedWords(now - compactingSince)}` : ""}</span>
      ) : start ? (
        <span className="turn-activity">Claude Code is handling: {revealedText(causeText)}</span>
      ) : <TurnActivity phase={phase} clock={turnClock} />)}
      {pendingTool !== null ? (
        <span className="activity-card">
          {/* Enter on the cursor's card or Alt-n enters card mode; neither answers by itself. */}
          ⚑ {revealedText(pendingTool)} needs approval
          {cardEntry === "held" ? "" : cardEntry === "on-card" ? " — Enter or Alt-n: card mode, then 1-3" : " — Alt-n: card mode, then 1-3"}
        </span>
      ) : (
        special === null && !start && queued > 0 && <span className="activity-keys">{queued} queued · </span>
      )}
      {/* Stays reachable even while a card waits (`j` past the last row lands here, `withACardAndStop`'s
          own fixture name) -- the card takes over the LINE's message, not this control. */}
      {canInterrupt && (
        <button type="button" className="stop" onClick={onInterrupt}>
          {limit ? "ctrl+c cancel" : "ctrl+c interrupt"}
        </button>
      )}
    </div>
  );
}
