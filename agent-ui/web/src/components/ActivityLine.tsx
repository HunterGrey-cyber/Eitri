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
  /** The tool name of the oldest pending permission card, or `null` when none waits. While it is
   *  set, this line names the card rather than the queue count -- a card already means the queue
   *  cannot flush, so the two are never both worth reading. */
  pendingTool?: string | null;
  /** The card row reads differently by mode -- `a`/`d`
   *  answer directly in BROWSE, but INPUT's keys go to the composer, so it says `Esc` first.
   *  Defaulted to `"browse"` for existing callers that never cared before this. */
  mode?: PanelMode;
};

/** The old `StatusLine`'s body that was about the turn --
 *  the in-flight motion indicator and Stop -- now its own line, only while a turn runs, directly
 *  above the composer. The mode block, the session status word and the position counter moved to
 *  `Footer`/`StatusRow`, and `data-nav-stop="status"` moves with the content it used to gate on
 *  ("the status line, only while Stop shows") rather than staying behind on an element with
 *  nothing left to answer for.
 *
 *  Restyled to one mono line, Claude Code's own
 *  `✻ Working… 1m 12s · ctrl+c interrupt` -- the interrupt words ARE the Stop button now (the
 *  mouse route stays), not a separate span beside it. */
export function ActivityLine({ state, turnClock = null, canInterrupt, onInterrupt, queued = 0, pendingTool = null, mode = "browse" }: Props) {
  // Moved out of `SessionHeader.tsx`: a terminal status wins over
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
  const phase = phaseOf(state);
  // The star is the line's one moving thing, and it moves only while the agent is working: a card
  // waiting, or a phase that is the user's to end, holds it still (`index.css`'s `turn-breath`).
  const moving = pendingTool === null && phase.kind !== "blocked";
  return (
    <div className="activity-line" data-nav-stop="status">
      <span className="activity-glyph" aria-hidden="true" data-moving={moving ? "true" : undefined}>
        ✻
      </span>
      {/* A card waiting IS what the line says: the phase word beside it only said
          "waiting for you" again, and at 520px the two were cut together, losing the keys that
          answer the card (seen in a GUI pass, 2026-09-26). */}
      {pendingTool === null && <TurnActivity phase={phase} clock={turnClock} />}
      {pendingTool !== null ? (
        <span className="activity-card">
          {/* The old "a / d" / "Esc, then a / d"
              said neither that `a`/`d` answer only the card under the cursor (`nav.ts`'s
              `permissionTarget` -- the old "the only card, from any row" fallback is gone) nor
              that a key answers only on its own, `TYPING_GUARD_MS` after the last one -- so a
              user who followed it literally, cursor elsewhere, or `a` a beat too soon after `Esc`,
              got "no card here" or the typing flash with no clue why. "j to it" points at the
              route back (`j`/`k`/`G` onto the card); "alone" is `TYPING_FLASH`'s own word for that rule. */}
          ⚑ {pendingTool} needs approval — {mode === "input" ? "Esc, j to it, a / d alone" : "j to it, a / d alone"}
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
