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

/** The name `workToStop` gives Claude Code's work the chat cannot show (no turn of the chat's own is open): no turn
 *  id can be it, so a question asked about that work never matches a turn that began after it. */
export const UNSEEN_WORK = "\u0000unseen-cli-work";

/** What the stop-this-turn question is about: the running turn's id, else, while Claude Code works where this chat
 *  cannot show it, `UNSEEN_WORK` (the chat's interrupt stops that work as it stops a turn); `null` when neither runs. */
export function workToStop(state: { activeTurnId: string | null; cliUnseenBusy: boolean }): string | null {
  return state.activeTurnId ?? (state.cliUnseenBusy ? UNSEEN_WORK : null);
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
 *  y/n guard counts (`counts`: `bypassYesCounts`), asked of the same tab while the turn it asked about (`askedTurn`)
 *  is still the one running (`turnNow`). The interrupt names no turn, so a `y` after that turn ended and another
 *  began would stop one nobody was asked about. Every other key puts the question away and stops nothing. */
export function stopTurnAnswer(
  key: { key: string; plain: boolean; repeat: boolean },
  counts: boolean,
  still: { sameTab: boolean; askedTurn: string; turnNow: string | null },
): StopTurnAnswer {
  if (key.key !== "y" && key.key !== "Y") return { kind: "cancel" };
  if (!key.plain || key.repeat || !counts) {
    return { kind: "refused", flash: "y must be pressed on its own to stop the turn; nothing was stopped" };
  }
  if (!still.sameTab) return { kind: "refused", flash: "the tab changed; nothing was stopped" };
  if (still.turnNow !== still.askedTurn) return { kind: "refused", flash: "the turn has ended; nothing was stopped" };
  return { kind: "interrupt" };
}
