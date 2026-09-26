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

/** Show the turn's motion indicator and Stop directly above the composer, only while a turn runs.
 *  Mode, session status, and position live in `Footer`/`StatusRow`. Keep `data-nav-stop="status"`
 *  with the turn controls. The lowercase `ctrl+c interrupt` text is the Stop button itself,
 *  preserving mouse activation. */
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
      <TurnActivity phase={phaseOf(state)} clock={turnClock} />
      {pendingTool !== null ? (
        <span className="activity-card">
          ⚑ {pendingTool} needs approval — {mode === "input" ? "Esc, then a / d" : "a / d"}
        </span>
      ) : (
        queued > 0 && <span className="activity-keys">{queued} queued · </span>
      )}
      {/* Stays reachable even while a card waits (`j` past the last row lands here, `withACardAndStop`'s
          own fixture name) -- the card takes over the LINE's message, not this control. */}
      {canInterrupt && (
        <button type="button" className="stop" onClick={onInterrupt}>
          ctrl+c interrupt
        </button>
      )}
    </div>
  );
}
