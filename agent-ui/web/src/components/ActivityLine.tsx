import type { AgentUiState, TurnClock } from "../types";
import type { PanelMode } from "../keymap";
import { phaseOf } from "../turnPhase";
import { TurnActivity } from "./TurnActivity";

type Props = {
  state: AgentUiState;
  /** How long the current turn has been running, tracked in `App.tsx` (`TurnClock`) rather than in
   *  `AgentUiState` -- see that type's own doc comment. `null` before any turn has started, and
   *  optional/defaulted for callers (existing tests, mainly) that render no in-flight indicator at
   *  all. Only read while a turn is actually working; see `TurnActivity`. */
  turnClock?: TurnClock | null;
  /** From the provider's advertised capabilities, never from the backend's name -- the same rule
   *  `Composer`'s own Stop button follows. */
  canInterrupt: boolean;
  onInterrupt: () => void;
  /** How many items are queued behind the running turn. Shown beside the
   *  interrupt key, never with `pendingTool`, which takes over the same slot instead. */
  queued?: number;
  /** The oldest pending permission card's tool name, or `null` if none waits.
   *  Replaces the queue count: a waiting card already prevents the queue from flushing. */
  pendingTool?: string | null;
  /** The card hint names `a`/`d` in BROWSE. In INPUT it says Escape first because keys go to the
   *  composer. Defaults to browse for callers that omit the mode. */
  mode?: PanelMode;
};

/** V2: the old `StatusLine`'s body that was about the turn --
 *  the in-flight motion indicator and Stop -- now its own line, only while a turn runs, directly
 *  above the composer. The mode block, the session status word and the position counter moved to
 *  `Footer`/`StatusRow`, and `data-nav-stop="status"` moves with the content it used to gate on
 *  ("the status line, only while Stop shows") rather than staying behind on an element with
 *  nothing left to answer for.
 *
 *  The compact Stop control leaves room for the current action, including on narrow panels. */
export function ActivityLine({ state, turnClock = null, canInterrupt, onInterrupt, queued = 0, pendingTool = null, mode = "browse" }: Props) {
  // Moved out of `SessionHeader.tsx` (panel-as-document task 6): a terminal status wins over
  // activeTurnId. `reducer.ts:177,184` (`session_unavailable`/`session_closed`) DO clear
  // activeTurnId -- an earlier note here argued the opposite, that nothing should clear it because
  // no provider event says "that turn is over", and that was wrong in its consequence: it left a
  // dead session reading "working" forever, in the very styling written for the status text it was
  // hiding, and kept `App.tsx`'s composer spinner and the supervisor dashboard's Working dot stuck
  // too. Clearing it invents no completion; the terminal status is still the thing being shown.
  // This guard is not load-bearing for THOSE two events any more, then -- it is what keeps a
  // session that dies some OTHER way (nothing here has found one) from reading "working" forever.
  const working = state.status.kind === "running" && state.activeTurnId !== null;
  if (!working) return null;
  return (
    <div className="activity-line" data-nav-stop="status">
      <span className="activity-glyph" aria-hidden="true">
        ✻
      </span>
      {/* A card waiting IS what the line says: the phase word beside it only said
          "waiting for you" again, and at 520px the two were cut together, losing the keys that
          answer the card (r2-gui GUI pass, 2026-09-26). */}
      {pendingTool === null && <TurnActivity phase={phaseOf(state)} clock={turnClock} />}
      {pendingTool !== null ? (
        <span className="activity-card">
          {/* The hint names both requirements for answering: move the cursor onto the card with
              `j`/`k`/`G`, then press `a`/`d` alone after `TYPING_GUARD_MS` without a key.
              Without those details, a misplaced cursor or a key too soon after `Esc` produces
              a refusal the hint does not explain. */}
          ⚑ {pendingTool} needs approval — {mode === "input" ? "Esc, j to it, a / d alone" : "j to it, a / d alone"}
        </span>
      ) : (
        queued > 0 && <span className="activity-keys">{queued} queued · </span>
      )}
      {/* Stays reachable even while a card waits (`j` past the last row lands here, `withACardAndStop`'s
          own fixture name) -- the card takes over the LINE's message, not this control. */}
      {canInterrupt && (
        <button type="button" className="stop" aria-label="Stop agent (Ctrl+C)" title="Stop agent (Ctrl+C)" onClick={onInterrupt}>
          Stop
        </button>
      )}
    </div>
  );
}
