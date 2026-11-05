import { describe, expect, it } from "vitest";
import { STOP_TURN_QUESTION, UNSEEN_WORK, stopAskFor, stopTurnAnswer, workToStop } from "./stopAsk";

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
  const still = { sameTab: true, askedTurn: "t1", turnNow: "t1" };

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
    expect(stopTurnAnswer(plainY, true, { ...still, turnNow: null }).kind).toBe("refused");
    expect(stopTurnAnswer(plainY, true, { ...still, sameTab: false }).kind).toBe("refused");
  });

  it("a y after another turn took the asked one's place stops nothing", () => {
    expect(stopTurnAnswer(plainY, true, { ...still, turnNow: "t2" })).toEqual({
      kind: "refused",
      flash: "the turn has ended; nothing was stopped",
    });
  });

  it("n_and_esc_do_nothing: every other key puts the question away", () => {
    for (const key of ["n", "N", "Escape", "a", "Enter"]) {
      expect(stopTurnAnswer({ key, plain: true, repeat: false }, true, still)).toEqual({ kind: "cancel" });
    }
  });
});

describe("what the stop-this-turn question is about", () => {
  it("a running turn, else Claude Code's unseen work, else nothing", () => {
    expect(workToStop({ activeTurnId: "t1", cliUnseenBusy: true })).toBe("t1");
    expect(workToStop({ activeTurnId: null, cliUnseenBusy: true })).toBe(UNSEEN_WORK);
    expect(workToStop({ activeTurnId: null, cliUnseenBusy: false })).toBeNull();
  });
});
