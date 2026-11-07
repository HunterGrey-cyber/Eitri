import { describe, expect, it } from "vitest";
import { initialState } from "./reducer";
import { initialPageModel, pageModel } from "./page_model";
import { stopTurnAnswer, workToStop } from "./stopAsk";
import type { Envelope, PageModel } from "./page_model";
import type { InboundPayload } from "./bridge";

const tab = (id: number) => ({
  id, number: id, label: `${id}`, name: null, state: "live" as const, mode: "auto" as const,
  marker: null, pending: 0, resumable: false, failure: null, title: null,
});

function receive(model: PageModel, payload: InboundPayload, receivedAt = 100): PageModel {
  return pageModel(model, { ...payload, receivedAt, monotonicAt: receivedAt / 10 });
}

function connected(): PageModel {
  return receive(initialPageModel(), { kind: "tabs", active: 1, tabs: [tab(1), tab(2)], defaultMode: "auto" });
}

describe("pageModel", () => {
  it("starts with review hints off until a host sends settings", () => {
    expect(initialPageModel().reviewSettings).toEqual({ enabled: false, hint: "off" });
  });

  it.each([
    ["key", "key"], ["full", "full"], [true, "full"], [false, "off"],
    ["off", "off"], [undefined, "off"], ["garbage", "off"], [null, "off"], [1, "off"],
  ])("reads review hint %j as %s", (hint, expected) => {
    const payload = { kind: "review_settings", enabled: true, ...(hint === undefined ? {} : { hint }) } as InboundPayload;
    const model = receive(connected(), payload);
    expect(model.reviewSettings).toEqual({ enabled: true, hint: expected });
    expect(receive(model, { kind: "review_settings", enabled: false, hint: "full" }).reviewSettings)
      .toEqual({ enabled: false, hint: "full" });
  });

  it("tabs_then_snapshot_in_one_batch_shows_the_new_active_tab", () => {
    const previous = pageModel(initialPageModel(), {
      kind: "tabs", active: 1, tabs: [tab(1), tab(2)], defaultMode: "auto", receivedAt: 100,
    });
    const snapshot = { ...initialState(), activeTurnId: "new-turn", status: { kind: "running" as const } };
    const switched = pageModel(previous, {
      kind: "tabs", active: 2, tabs: [tab(1), tab(2)], defaultMode: "auto", receivedAt: 200,
    });
    const shown = pageModel(switched, {
      kind: "snapshot", tab: 2, state: snapshot, throughRevision: 1, receivedAt: 200,
    });
    expect(shown.tabs?.active).toBe(2);
    expect(shown.state.activeTurnId).toBe("new-turn");
    expect(shown.turnClock).toEqual({ turnId: "new-turn", since: 200, exact: false });
    expect(pageModel(shown, {
      kind: "snapshot", tab: 1, state: initialState(), throughRevision: 2, receivedAt: 300,
    })).toBe(shown);
    expect(previous.tabs?.active).toBe(1);
    expect(previous.state.activeTurnId).toBeNull();
  });

  it("an_unknown_envelope_kind_leaves_the_model_unchanged", () => {
    const model = initialPageModel();
    expect(pageModel(model, { kind: "unknown", receivedAt: 100 } as unknown as Envelope)).toBe(model);
  });

  it("a_snapshot_keeps_an_exact_clock_and_can_replace_an_estimated_start", () => {
    const envelope = {
      kind: "snapshot" as const, tab: 1, throughRevision: 1,
      state: { ...initialState(), activeTurnId: "turn" },
    };
    const estimated = receive(connected(), envelope, 200);
    const exact = receive(estimated, { ...envelope, turnStartedAtMs: 50 }, 300);
    const resynced = receive(exact, { ...envelope, turnStartedAtMs: 60 }, 400);
    expect(estimated.turnClock).toEqual({ turnId: "turn", since: 200, exact: false });
    expect(exact.turnClock).toEqual({ turnId: "turn", since: 50, exact: true });
    expect(resynced.turnClock).toBe(exact.turnClock);
    expect(receive(resynced, { ...envelope, state: initialState() }).turnClock).toBeNull();
  });

  it("a_failed_conversation_survives_a_switch_and_is_released_when_restarted", () => {
    const running = receive(connected(), {
      kind: "events", tab: 1, fromRevision: 0, throughRevision: 2,
      events: [{ type: "user_prompt_submitted", text: "keep this conversation" }, { type: "turn_started", turn_id: "turn" }],
    });
    const failed = receive(Object.freeze(running), { kind: "error", tab: 1, message: "connection closed" });
    expect(running.keptStates.size).toBe(0);
    expect(failed.state.userPrompts[0].text).toBe("keep this conversation");
    expect(failed.state.activeTurnId).toBeNull();
    const tabs = [{ ...tab(1), state: "failed" as const }, tab(2)];
    const away = receive(failed, { kind: "tabs", active: 2, tabs, defaultMode: "auto" });
    const back = receive(away, { kind: "tabs", active: 1, tabs, defaultMode: "auto" });
    expect(back.state).toBe(failed.state);
    const restarted = receive(back, {
      kind: "tabs", active: 1, tabs: [{ ...tab(1), state: "starting" }, tab(2)], defaultMode: "auto",
    });
    expect(restarted.keptStates.size).toBe(0);
    expect(restarted.state.userPrompts).toEqual([]);
    expect(failed.keptStates.get(1)).toBe(failed.state);
  });

  it("a_handoff_reply_clears_only_its_own_request_without_mutating_bookkeeping", () => {
    const model: PageModel = {
      ...connected(),
      inFlight: new Map([["old", { kind: "handoff", tab: 1 }], ["other", { kind: "handoff", tab: 2 }]]),
      handoffRequests: new Map([[1, "new"], [2, "other"]]),
    };
    const result = receive(Object.freeze(model), { kind: "command_result", requestId: "old", ok: true });
    expect(result.inFlight.has("old")).toBe(false);
    expect(result.inFlight.has("other")).toBe(true);
    expect(result.handoffRequests).toEqual(model.handoffRequests);
    expect(model.inFlight.has("old")).toBe(true);
  });

  it("a_bypass_reprompt_replaces_its_nonce_and_restarts_its_on_screen_wait", () => {
    const envelope = { kind: "confirm_bypass" as const, tab: 1, scope: "tab" as const, nonce: 1, lines: ["Allow?"] };
    const first = receive(connected(), envelope, 100);
    const next = receive(first, { ...envelope, nonce: 2 }, 200);
    expect(first.confirm).toMatchObject({ nonce: 1, openedAt: 10 });
    expect(next.confirm).toMatchObject({ nonce: 2, openedAt: 20 });
    expect(receive(first, { ...envelope, nonce: 2 }, 200)).toEqual(next);
  });

  it("a_permission_landing_for_another_tab_cannot_cancel_a_waiting_answer", () => {
    const model = connected();
    expect(receive(model, { kind: "focus_permission", tab: 2 })).toBe(model);
    const landed = receive(model, { kind: "focus_permission", tab: 1 });
    expect(landed.answerEpoch).toBe(model.answerEpoch + 1);
    expect(landed.permissionRequest).toBe(model.permissionRequest + 1);
    expect(receive(landed, { kind: "editor_typing", typing: true, periodMs: 200 })).toBe(landed);
  });

  it("a_new_key_table_and_a_switch_move_the_answer_epoch_but_a_repeated_tabs_does_not", () => {
    const model = connected();
    const table = receive(model, {
      kind: "keymap", prefix: "Ctrl+a", window: [], prefixKeys: [], panel: model.keymapHelp.panel, newTabChord: "",
    });
    expect(table.answerEpoch, "a deferred key closed over the old table's binding").toBe(model.answerEpoch + 1);
    const same = receive(model, { kind: "tabs", active: 1, tabs: [tab(1), tab(2)], defaultMode: "auto" });
    expect(same.answerEpoch, "the same active tab leaves a waiting answer aimed at its card").toBe(model.answerEpoch);
    const switched = receive(model, { kind: "tabs", active: 2, tabs: [tab(1), tab(2)], defaultMode: "auto" });
    expect(switched.answerEpoch, "a waiting answer was aimed at the old tab's card").toBe(model.answerEpoch + 1);
  });
});

describe("the stop question across a rebuilt conversation", () => {
  const plainY = { key: "y", plain: true, repeat: false };
  const unseenSnapshot = (throughRevision: number): InboundPayload => ({
    kind: "snapshot", tab: 1, throughRevision, state: { ...initialState(), status: { kind: "running" }, cliUnseenBusy: true },
  });
  /** Tab 1 with Claude Code working out of sight, and the stop question up about that stretch. */
  function askedAboutUnseenWork(): PageModel {
    const shown = receive(connected(), unseenSnapshot(1));
    const work = workToStop(shown.state);
    expect(work?.kind).toBe("unseen");
    return { ...shown, confirm: { kind: "stop_turn", tab: 1, work: work!, lines: [], openedAt: 0 } };
  }
  const restartedWithUnseenWork = (model: PageModel) =>
    receive(
      receive(model, { kind: "tabs", active: 1, tabs: [{ ...tab(1), state: "starting" }, tab(2)], defaultMode: "auto" }),
      unseenSnapshot(1),
    );
  const answerAfter = (asked: PageModel, now: PageModel) => {
    if (asked.confirm?.kind !== "stop_turn") throw new Error("no stop question");
    return stopTurnAnswer(plainY, true, { sameTab: true, asked: asked.confirm.work, now: workToStop(now.state) });
  };

  const endings: [string, InboundPayload][] = [
    ["an error with nothing shown", { kind: "error", tab: 1, message: "connection closed" }],
    ["a handoff", { kind: "handoff", tab: 1, command: "claude --resume s", cwd: "/p", providerSessionId: "s" }],
  ];
  for (const [name, ending] of endings) {
    it(`${name} puts the stop question away and never counts the unseen work down`, () => {
      const asked = askedAboutUnseenWork();
      const ended = receive(asked, ending);
      // Even had the question stayed up, the next session's unseen work is not the work it asked about.
      const again = restartedWithUnseenWork(ended);
      expect(answerAfter(asked, again).kind).toBe("refused");
      expect(again.state.cliUnseenEpisode).toBeGreaterThan(asked.state.cliUnseenEpisode);
      expect(ended.state.cliUnseenEpisode).toBeGreaterThanOrEqual(asked.state.cliUnseenEpisode);
      expect(ended.confirm).toBeNull();
    });
  }

  it("a switch away and back never counts the unseen work down", () => {
    const asked = askedAboutUnseenWork();
    const away = receive(asked, { kind: "tabs", active: 2, tabs: [tab(1), tab(2)], defaultMode: "auto" });
    const back = receive(receive(away, { kind: "tabs", active: 1, tabs: [tab(1), tab(2)], defaultMode: "auto" }), unseenSnapshot(2));
    expect(answerAfter(asked, back).kind).toBe("refused");
    expect(away.state.cliUnseenEpisode).toBeGreaterThanOrEqual(asked.state.cliUnseenEpisode);
  });
});
