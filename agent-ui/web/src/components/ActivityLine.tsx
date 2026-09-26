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
  /** How many items are queued behind the running turn (phase 3 ruling 1). Shown beside the
   *  interrupt key, never with `pendingTool`, which takes over the same slot instead. */
  queued?: number;
  /** The tool name of the oldest pending permission card, or `null` when none waits (phase 3 P1,
   *  ruling 26). While it is set, this line names the card rather than the queue count -- a card
   *  already means the queue cannot flush (ruling 4), so the two are never both worth reading. */
  pendingTool?: string | null;
  /** Panel round 2 (plan Task 10; spec §5.1): the card row reads differently by mode -- `a`/`d`
   *  answer directly in BROWSE, but INPUT's keys go to the composer, so it says `Esc` first.
   *  Defaulted to `"browse"` for existing callers that never cared before this. */
  mode?: PanelMode;
};

/** V2 (session tabs spec §3.3, ruling 8): the old `StatusLine`'s body that was about the turn --
 *  the in-flight motion indicator and Stop -- now its own line, only while a turn runs, directly
 *  above the composer. The mode block, the session status word and the position counter moved to
 *  `Footer`/`StatusRow`, and `data-nav-stop="status"` moves with the content it used to gate on
 *  ("the status line, only while Stop shows") rather than staying behind on an element with
 *  nothing left to answer for.
 *
 *  Panel round 2 (plan Task 10; spec §5.1): restyled to one mono line, Claude Code's own
 *  `✻ Working… 1m 12s · ctrl+c interrupt` -- the interrupt words ARE the Stop button now (the
 *  mouse route N1 keeps), not a separate span beside it. */
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
      {/* A card waiting IS what the line says (spec §5.1): the phase word beside it only said
          "waiting for you" again, and at 520px the two were cut together, losing the keys that
          answer the card (r2-gui GUI pass, 2026-09-26). */}
      {pendingTool === null && <TurnActivity phase={phaseOf(state)} clock={turnClock} />}
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
