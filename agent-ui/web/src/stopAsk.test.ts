import { describe, expect, it } from "vitest";
import { STOP_TURN_QUESTION, sameWork, stopAskFor, stopTurnAnswer, workToStop, type StopWork } from "./stopAsk";

describe("which question a stop arrival asks", () => {
  it("stop_intent_shows_stop_this_turn_when_a_turn_runs: a turn wins over background work", () => {
    expect(stopAskFor({ live: true, turnRunning: true, background: 0 })).toBe("turn");
    expect(stopAskFor({ live: true, turnRunning: true, background: 3 })).toBe("turn");
    expect(STOP_TURN_QUESTION).toBe("Stop this turn? (y/n)");
  });

  it("stop_intent_shows_stop_background_when_only_background_runs", () => {
    expect(stopAskFor({ live: true, turnRunning: false, background: 2 })).toBe("background");
  });

  it("a tab with nothing running, or no live session, asks nothing", () => {
    expect(stopAskFor({ live: true, turnRunning: false, background: 0 })).toBe("nothing");
    expect(stopAskFor({ live: false, turnRunning: true, background: 2 })).toBe("nothing");
  });
});

describe("the stop-this-turn question's answer", () => {
  const plainY = { key: "y", plain: true, repeat: false };
  const t1: StopWork = { kind: "turn", turnId: "t1" };
  const still = { sameTab: true, asked: t1, now: t1 as StopWork | null };

  it("y_after_the_guard_interrupts: a plain, unrepeated y past the guard, the turn still running", () => {
    expect(stopTurnAnswer(plainY, true, still)).toEqual({ kind: "interrupt" });
    expect(stopTurnAnswer({ ...plainY, key: "Y" }, true, still)).toEqual({ kind: "interrupt" });
  });

  it("y_inside_the_yn_guard_is_refused: too soon, repeated or with a modifier stops nothing", () => {
    for (const [key, counts] of [
      [plainY, false],
      [{ ...plainY, repeat: true }, true],
      [{ ...plainY, plain: false }, true],
    ] as const) {
      const answer = stopTurnAnswer(key, counts, still);
      expect(answer.kind).toBe("refused");
    }
  });

  it("a y after the turn ended or the tab changed stops nothing", () => {
    expect(stopTurnAnswer(plainY, true, { ...still, now: null }).kind).toBe("refused");
    expect(stopTurnAnswer(plainY, true, { ...still, sameTab: false }).kind).toBe("refused");
  });

  it("a y after another turn took the asked one's place stops nothing", () => {
    expect(stopTurnAnswer(plainY, true, { ...still, now: { kind: "turn", turnId: "t2" } })).toEqual({
      kind: "refused",
      flash: "the turn has ended; nothing was stopped",
    });
  });

  it("a y asked about one stretch of unseen work stops nothing once another stretch, or a turn, runs instead", () => {
    const a: StopWork = { kind: "unseen", episode: 1 };
    const asked = { ...still, asked: a, now: a as StopWork | null };
    expect(stopTurnAnswer(plainY, true, asked)).toEqual({ kind: "interrupt" });
    for (const now of [{ kind: "unseen", episode: 2 }, { kind: "turn", turnId: "t1" }, null] as const) {
      expect(stopTurnAnswer(plainY, true, { ...asked, now })).toEqual({
        kind: "refused",
        flash: "the work changed since the question; nothing was stopped",
      });
    }
  });

  it("n_and_esc_do_nothing: every other key puts the question away", () => {
    for (const key of ["n", "N", "Escape", "a", "Enter"]) {
      expect(stopTurnAnswer({ key, plain: true, repeat: false }, true, still)).toEqual({ kind: "cancel" });
    }
  });
});

describe("what the stop-this-turn question is about", () => {
  it("a running turn, else Claude Code's unseen work by its stretch's number, else nothing", () => {
    expect(workToStop({ activeTurnId: "t1", cliUnseenBusy: true, cliUnseenEpisode: 3 })).toEqual({ kind: "turn", turnId: "t1" });
    expect(workToStop({ activeTurnId: null, cliUnseenBusy: true, cliUnseenEpisode: 3 })).toEqual({ kind: "unseen", episode: 3 });
    expect(workToStop({ activeTurnId: null, cliUnseenBusy: false, cliUnseenEpisode: 3 })).toBeNull();
  });

  it("no turn id is ever the same work as a stretch of unseen work, whatever the id holds", () => {
    for (const turnId of ["1", "3", "", "\u0000unseen-cli-work", "unseen", "[object Object]"]) {
      const turn: StopWork = { kind: "turn", turnId };
      for (const episode of [0, 1, 3]) {
        const unseen: StopWork = { kind: "unseen", episode };
        expect(sameWork(turn, unseen)).toBe(false);
        expect(sameWork(unseen, turn)).toBe(false);
      }
      expect(sameWork(turn, { kind: "turn", turnId })).toBe(true);
    }
    expect(sameWork({ kind: "unseen", episode: 1 }, { kind: "unseen", episode: 1 })).toBe(true);
    expect(sameWork({ kind: "unseen", episode: 1 }, { kind: "unseen", episode: 2 })).toBe(false);
  });
});
