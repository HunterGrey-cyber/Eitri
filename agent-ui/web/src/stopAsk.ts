/**
 * Stop, asked in the chat. When the host brings a tab forward to be stopped (`arrive` with `intent: "stop"`), the
 * decision is still made here, where the work is visible: the chat asks its own y/n question in the band, and only
 * a counted `y` sends the chat's own stop message for that tab. The arrival itself stops nothing.
 */

/** The question while a turn runs. Background work left by the agent gets the stop-background question instead. */
export const STOP_TURN_QUESTION = "Stop this turn? (y/n)";

/** Said when a stop arrives at a tab with nothing running (the host refuses one it sees as idle; the work may have
 *  ended since it looked). */
export const NOTHING_TO_STOP = "nothing is running in this tab: nothing to stop";

/** What the stop-this-turn question is about: a turn of the chat's own, by its id, or one stretch of Claude Code's
 *  work the chat cannot show (no turn of the chat's own is open), by the number the page gave that stretch when it
 *  began (`cliUnseenEpisode`). The two kinds never compare equal, whatever a turn id holds, so a question asked about
 *  one never matches a turn, or another stretch of unseen work, that began after it. */
export type StopWork = { kind: "turn"; turnId: string } | { kind: "unseen"; episode: number };

/** The work the stop-this-turn question would be about now: the running turn, else, while Claude Code works where
 *  this chat cannot show it, that stretch of work (the chat's interrupt stops it as it stops a turn); `null` when
 *  neither runs. */
export function workToStop(state: {
  activeTurnId: string | null;
  cliUnseenBusy: boolean;
  cliUnseenEpisode: number;
}): StopWork | null {
  if (state.activeTurnId !== null) return { kind: "turn", turnId: state.activeTurnId };
  return state.cliUnseenBusy ? { kind: "unseen", episode: state.cliUnseenEpisode } : null;
}

/** Whether `now` is the very work `asked` named: the same turn, or the same stretch of unseen work. */
export function sameWork(asked: StopWork, now: StopWork | null): boolean {
  if (now === null) return false;
  if (asked.kind === "turn") return now.kind === "turn" && now.turnId === asked.turnId;
  return now.kind === "unseen" && now.episode === asked.episode;
}

/** What a stop arrival asks: a running turn comes first, since stopping it is what the person sees happening. */
export type StopAsk = "turn" | "background" | "nothing";

export function stopAskFor(facts: { live: boolean; turnRunning: boolean; background: number }): StopAsk {
  if (!facts.live) return "nothing";
  if (facts.turnRunning) return "turn";
  return facts.background > 0 ? "background" : "nothing";
}

export type StopTurnAnswer = { kind: "interrupt" } | { kind: "refused"; flash: string } | { kind: "cancel" };

/** The answer one key gives the stop-this-turn question. Only `y`/`Y` can stop, and only a plain, unrepeated one the
 *  y/n guard counts (`counts`: `bypassYesCounts`), asked of the same tab while the work it asked about (`asked`: a
 *  turn or a stretch of unseen work) is still the work running (`now`, `workToStop` at the key). The interrupt names
 *  no turn, so a `y` after that work ended and other work began would stop work nobody was asked about. Every other
 *  key puts the question away and stops nothing. */
export function stopTurnAnswer(
  key: { key: string; plain: boolean; repeat: boolean },
  counts: boolean,
  still: { sameTab: boolean; asked: StopWork; now: StopWork | null },
): StopTurnAnswer {
  if (key.key !== "y" && key.key !== "Y") return { kind: "cancel" };
  if (!key.plain || key.repeat || !counts) {
    return { kind: "refused", flash: "y must be pressed on its own to stop the turn; nothing was stopped" };
  }
  if (!still.sameTab) return { kind: "refused", flash: "the tab changed; nothing was stopped" };
  if (!sameWork(still.asked, still.now)) {
    // A stretch of unseen work also changes its number on a snapshot while the same work may still run.
    const flash = still.asked.kind === "turn"
      ? "the turn has ended; nothing was stopped"
      : "the work changed since the question; nothing was stopped";
    return { kind: "refused", flash };
  }
  return { kind: "interrupt" };
}
