import { useEffect, useState } from "react";
import { phaseWord, type TurnPhase } from "../turnPhase";
import type { TurnClock } from "../types";

type Props = {
  phase: TurnPhase;
  /** `null` transiently or when the backend/build does not supply a clock.
   *  Omit the elapsed-time reading in that case rather than guessing. */
  clock: TurnClock | null;
};

/** Show the phase word and elapsed clock while the caller's `ActivityLine.working` predicate is true.
 * This component relies on the caller to determine whether a turn is in flight.
 * `ActivityLine` supplies the motion through its star, which holds still while `phase.kind === "blocked"`
 * because permission is pending and the agent is not working. The phase word and clock still render. */
export function TurnActivity({ phase, clock }: Props) {
  // Tick once a second in this component so the elapsed clock does not re-render the whole panel
  // on top of streaming updates. The state value is unused; it only forces `elapsed` to be
  // recomputed on the next tick.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (clock === null) return;
    const id = window.setInterval(() => setTick((n) => n + 1), 1000);
    // Clear the interval when the effect runs again and on unmount, so removing the indicator
    // also stops its timer.
    return () => window.clearInterval(id);
  }, [clock?.turnId]);

  const elapsed =
    clock === null ? null : `${Math.max(0, Math.floor((Date.now() - clock.since) / 1000))}s${clock.exact ? "" : "+"}`;

  return (
    <span className="turn-activity" data-phase={phase.kind}>
      <span className="turn-state">{phaseWord(phase)}</span>
      {elapsed !== null && <span className="turn-elapsed">{elapsed}</span>}
    </span>
  );
}
