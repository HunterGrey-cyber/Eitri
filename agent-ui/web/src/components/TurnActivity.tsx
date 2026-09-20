import { useEffect, useState } from "react";
import { phaseWord, type TurnPhase } from "../turnPhase";
import type { TurnClock } from "../types";

type Props = {
  phase: TurnPhase;
  /** `null` transiently or when the backend/build does not supply a clock.
   *  Omit the elapsed-time reading in that case rather than guessing. */
  clock: TurnClock | null;
};

/**
 * The motion indicator for a running turn. The caller mounts it only while `StatusLine`'s
 * `working` predicate is true; this component does not derive that condition itself.
 *
 * Withhold `.meter` and `.meter-fill` while `phase.kind === "blocked"` because the agent is
 * waiting for permission. The phase word and elapsed clock still render; only the motion stops.
 */
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
      {phase.kind !== "blocked" && (
        <span className="meter" aria-hidden="true">
          <span className="meter-fill" />
        </span>
      )}
      <span className="turn-state">{phaseWord(phase)}</span>
      {elapsed !== null && <span className="turn-elapsed">{elapsed}</span>}
    </span>
  );
}
