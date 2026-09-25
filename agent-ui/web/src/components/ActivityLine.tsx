import type { AgentUiState, TurnClock } from "../types";
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
};

/** V2 (session tabs spec §3.3, ruling 8): the old `StatusLine`'s body that was about the turn --
 *  the in-flight motion indicator and Stop -- now its own line, only while a turn runs, directly
 *  above the composer. The mode block, the session status word and the position counter moved to
 *  `Footer`/`StatusRow`, and `data-nav-stop="status"` moves with the content it used to gate on
 *  ("the status line, only while Stop shows") rather than staying behind on an element with
 *  nothing left to answer for. */
export function ActivityLine({ state, turnClock = null, canInterrupt, onInterrupt }: Props) {
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
      <TurnActivity phase={phaseOf(state)} clock={turnClock} />
      {canInterrupt && (
        <button type="button" className="stop" onClick={onInterrupt}>
          Stop
        </button>
      )}
    </div>
  );
}
