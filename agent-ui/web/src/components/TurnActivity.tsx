import { useEffect, useState } from "react";
import { phaseWord, type TurnPhase } from "../turnPhase";
import type { TurnClock } from "../types";

type Props = {
  phase: TurnPhase;
  /** `null` only transiently, or on a backend/build that never wires the clock up -- rendered
   *  without an elapsed-time reading in that case rather than guessing at one. */
  clock: TurnClock | null;
};

/**
 * The in-flight motion indicator. A caller mounts this only while `ActivityLine`'s own `working`
 * predicate is true (`ActivityLine` took this over from the old `StatusLine`) -- this component does
 * not re-derive that condition,
 * so mounting it at all is the caller's claim that a turn is in flight.
 *
 * The phase word and the elapsed clock. The motion that says a turn is in flight is the activity line's
 * star (`ActivityLine`), which holds still while `phase.kind === "blocked"`: a pending permission means
 * the AGENT is not working, so animating a claim that it is would be a lie. The word and the
 * clock still render in that phase -- only the motion stops.
 */
export function TurnActivity({ phase, clock }: Props) {
  // One tick a second, local to this leaf: `setState` here re-renders only this component, not the
  // whole panel, which is what keeps a long-running turn's elapsed clock from adding a second
  // per-second re-render pass on top of the streaming pump. The value itself is never read;
  // it exists only to force React to recompute `elapsed` below on the next tick.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (clock === null) return;
    const id = window.setInterval(() => setTick((n) => n + 1), 1000);
    // Cleared on every re-run of this effect AND on unmount -- the latter is what makes "the
    // indicator is gone" and "no timer is left running" the same fact rather than two that could
    // drift apart.
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
