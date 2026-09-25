import { useEffect, useState } from "react";
import { phaseWord, type TurnPhase } from "../turnPhase";
import type { TurnClock } from "../types";

type Props = {
  phase: TurnPhase;
  /** `null` only transiently, or on a backend/build that never wires the clock up -- rendered
   *  without an elapsed-time reading in that case rather than guessing at one. See design doc
   *  §8.4. */
  clock: TurnClock | null;
};

/**
 * The in-flight motion indicator (`2026-09-20-in-flight-motion-design.md`). A caller mounts this
 * only while `ActivityLine`'s own `working` predicate is true (§5.1; `ActivityLine` moved this out
 * of `StatusLine`, V2, session tabs Task 10) -- this component does not re-derive that condition,
 * so mounting it at all is the caller's claim that a turn is in flight.
 *
 * The meter itself (`.meter` / `.meter-fill`) is withheld while `phase.kind === "blocked"`: a
 * pending permission means the AGENT is not working, so animating a claim that it is would be a
 * lie (§2, §3). The word and the elapsed clock still render in that phase -- only the motion stops.
 */
export function TurnActivity({ phase, clock }: Props) {
  // One tick a second, local to this leaf: `setState` here re-renders only this component, not the
  // whole panel, which is what keeps a long-running turn's elapsed clock from adding a second
  // per-second re-render pass on top of the streaming pump (§5.4). The value itself is never read;
  // it exists only to force React to recompute `elapsed` below on the next tick.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (clock === null) return;
    const id = window.setInterval(() => setTick((n) => n + 1), 1000);
    // Cleared on every re-run of this effect AND on unmount -- the latter is what makes "the
    // indicator is gone" and "no timer is left running" the same fact rather than two that could
    // drift apart (§5.1-§5.3).
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
