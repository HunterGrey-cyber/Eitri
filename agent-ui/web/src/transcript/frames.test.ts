import { describe, expect, it, vi } from "vitest";
import { initialState } from "../reducer";
import { applyFrame, initialTranscript, wantMore } from "./frames";

const key = "k-0123456789abcdef";
const host = { kind: "host", protocol: 1, deskProtocol: 1, app: "Eitri", device: "Pixel", location: "desk" };
const theme = { vars: { "--nv-bg": "#112233" }, scheme: "dark" };
const fixtures = import.meta.glob<unknown[]>("../fixtures/phone-frames/*.json", { eager: true, import: "default" });
const snapshot = (first = 2, generation = 1) => ({
  v: 1, kind: "snapshot", key, generation, through: 20, first_turn: first, cut: [],
  frame: { kind: "snapshot", state: { ...initialState(),
    userPrompts: [{ seq: 10, text: "two" }, { seq: 15, text: "three" }],
    transcript: [{ seq: 12, text: "cut" }, { seq: 17, text: "latest" }],
  } },
});
const page = (from = 0, through = 1, generation = 1) => ({
  v: 1, kind: "page", key, generation, from_turn: from, through_turn: through,
  frame: { kind: "snapshot", state: { ...initialState(),
    userPrompts: [{ seq: 0, text: "zero" }, { seq: 5, text: "one" }],
    transcript: [{ seq: 2, text: "oldest" }, { seq: 7, text: "older" }],
  } },
});
const events = (from = 20, generation = 1) => ({
  v: 1, kind: "events", key, generation, from, through: 22,
  frame: { kind: "events", events: [{ type: "model_reported", model: "new-model" }], revisions: [22] },
});
const ready = () => applyFrame(applyFrame(initialTranscript(), host), snapshot());

describe("phone frames", () => {
  it("snapshot_then_real_events_apply_without_a_snapshot_request", () => {
    const live = fixtures["../fixtures/phone-frames/live.json"];
    expect(live).toBeDefined();
    const frames = live as { kind: string; through?: number }[];
    expect(frames.filter((f) => f.kind === "events")).toHaveLength(2);
    let m = initialTranscript();
    for (const frame of frames.filter((f) => ["greeting", "snapshot", "events"].includes(f.kind))) {
      m = applyFrame(m, frame);
      expect(m.wantSnapshot).toBe(false);
    }
    expect(m.state.transcript.map((e) => e.text)).toEqual(["Hello phone"]);
    expect(m.throughRevision).toBe(frames.filter((f) => f.kind === "events").at(-1)?.through);
  });

  it("events_with_a_gap_ask_for_a_snapshot", () => {
    const before = ready();
    for (const frame of [events(21), events(20, 2), { ...events(), key: "other" }]) {
      const after = applyFrame(before, frame);
      expect(after.wantSnapshot).toBe(true);
      expect(after.state).toBe(before.state);
      expect(after.throughRevision).toBe(20);
    }
    const gap = applyFrame(before, events(21));
    expect(applyFrame(gap, events()).state).toBe(before.state);
    expect(applyFrame(gap, snapshot()).wantSnapshot).toBe(false);
  });

  it("pages_prepend_without_gap_or_duplicate", () => {
    const before = ready();
    const after = applyFrame(before, page());
    expect(after.firstTurn).toBe(0);
    expect(after.state.transcript.map((e) => e.seq)).toEqual([2, 7, 12, 17]);
    expect(applyFrame(after, page()).state).toEqual(after.state);
    expect(applyFrame(before, page(0, 0))).toBe(before);
    expect(applyFrame(before, { ...page(), key: "other" })).toBe(before);
    expect(before.state.transcript).toHaveLength(2);
    expect(after.throughRevision).toBe(20);
  });

  it("page_over_held_turns_replaces_the_cut_entry", () => {
    const before = applyFrame(initialTranscript(), { ...snapshot(), cut: [{ entry_id: "transcript:12", bytes: 5000 }] });
    const whole = { ...page(2, 2), frame: { state: { ...initialState(),
      userPrompts: [{ seq: 10, text: "two" }], transcript: [{ seq: 12, text: "whole reply" }],
    } } };
    const after = applyFrame(before, whole);
    expect(after.state.transcript).toEqual([{ seq: 12, text: "whole reply" }, { seq: 17, text: "latest" }]);
    expect(after.cut).toEqual([]);
    expect(after.state.userPrompts).toHaveLength(2);
    expect(after.firstTurn).toBe(2);
  });

  it("page_through_the_last_turn_applies_only_where_the_view_stands", () => {
    const open = { ...snapshot(), cut: [{ entry_id: "transcript:17", bytes: 5000 }] };
    open.frame.state = { ...open.frame.state, activeTurnId: "t3", assistantMessageOpen: true,
      transcript: [{ seq: 12, text: "cut" }, { seq: 17, text: "Hello" }] };
    const before = { ...applyFrame(applyFrame(initialTranscript(), host), open), owedPage: true };
    const whole = (through: number, text: string) => ({ ...page(3, 3), frame: { kind: "snapshot",
      throughRevision: through, state: { ...initialState(), userPrompts: [{ seq: 15, text: "three" }],
        transcript: [{ seq: 17, text }] } } });
    const delta = { ...events(20), through: 21, frame: { kind: "events", revisions: [21],
      events: [{ type: "content_delta", turn_id: "t3", kind: "text", text: " phone" }] } };
    // Read after the delta was folded and before it was sent: the delta would land on it again.
    const ahead = applyFrame(before, whole(21, "Hello phone"));
    expect(ahead.state).toBe(before.state);
    expect(ahead.cut).toEqual(before.cut);
    expect(ahead.owedPage).toBe(false);
    const next = applyFrame(ahead, delta);
    expect(next.wantSnapshot).toBe(false);
    expect(next.state.transcript.map((e) => e.text)).toEqual(["cut", "Hello phone"]);
    // Read where the view stands: it applies, and the delta after it lands once.
    const atView = applyFrame(before, whole(20, "Hello"));
    expect(atView.cut).toEqual([]);
    expect(applyFrame(atView, delta).state.transcript.map((e) => e.text)).toEqual(["cut", "Hello phone"]);
  });

  it("page_from_an_old_generation_is_dropped", () => {
    const before = { ...ready(), owedPage: true };
    expect(applyFrame(before, page(0, 1, 0))).toBe(before);
  });

  it("new_generation_snapshot_replaces_all", () => {
    const before = { ...applyFrame(ready(), page()), owedPage: true, ended: "lost", gone: true, wantSnapshot: true };
    const after = applyFrame(before, snapshot(2, 2));
    expect(after.generation).toBe(2);
    expect(after.state.userPrompts.map((p) => p.text)).toEqual(["two", "three"]);
    expect(after.firstTurn).toBe(2);
    expect(after.ended).toBeNull();
    expect(after.gone || after.wantSnapshot || after.owedPage).toBe(false);
  });

  it("unknown_kind_becomes_one_update_row", () => {
    const after = applyFrame(ready(), { v: 1, kind: "future" });
    expect(after.update).toBe(true);
    expect(after.stopped).toBe(false);
    expect(applyFrame(after, { v: 1, kind: "another" })).toBe(after);
  });

  it("frames_this_build_knows_keep_applying_after_an_update_row", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const unknownEvent = { ...events(), through: 21, frame: { events: [{ type: "future_event" }], revisions: [21] } };
    const badGreeting = { v: 1, kind: "greeting", key, greeting: { theme: "not tokens" } };
    for (const odd of [{ v: 1, kind: "future" }, unknownEvent, badGreeting, { v: 1, kind: "theme", theme: 3 }]) {
      let m = applyFrame(ready(), odd);
      expect(m.update).toBe(true);
      const from = m.throughRevision;
      m = applyFrame(m, { ...events(from), through: from + 2, frame: { ...events().frame, revisions: [from + 2] } });
      expect(m.wantSnapshot).toBe(false);
      expect(m.throughRevision).toBe(from + 2);
      expect(m.state.model).toBe("new-model");
      expect(m.update).toBe(true);
      expect(wantMore(m)).toEqual({ kind: "more", key, generation: 1, before_turn: 2 });
      m = applyFrame(m, { v: 1, kind: "gone", key, why: "closed" });
      expect(m.gone).toBe(true);
      m = applyFrame(m, snapshot(2, 2));
      expect(m.generation).toBe(2);
      expect(m.gone).toBe(false);
    }
    warn.mockRestore();
  });

  it("unknown_event_type_inside_a_frame_sets_update", () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const frame = events();
    const after = applyFrame(ready(), { ...frame, frame: { events: [{ type: "future_event" }] } });
    expect(after.update).toBe(true);
    const nested = (leaf: unknown) => applyFrame(ready(), { ...frame, frame: { events: [{ type: "subagent_frame",
      parent_tool_use_id: "tool-1", frame: { type: "subagent_frame", parent_tool_use_id: "tool-2", frame: leaf } }] } });
    const future = nested({ type: "future_event" });
    expect(future.update).toBe(true);
    expect(future.throughRevision).toBe(22);
    // A nested kind this build knows but does not render is dropped on purpose, without the row.
    expect(nested({ type: "model_reported", model: "inner" }).update).toBe(false);
    expect(nested({ type: "content_delta", turn_id: "t", kind: "text", text: "hi" }).update).toBe(false);
    warn.mockRestore();
  });

  it("protocol_mismatch_stops_applying", () => {
    for (const hello of [{ ...host, protocol: 2 }, { ...host, deskProtocol: 2 }]) {
      const after = applyFrame(ready(), hello);
      expect(after.update).toBe(true);
      expect(after.stopped).toBe(true);
      expect(wantMore(after)).toBeNull();
      expect(applyFrame(after, events())).toBe(after);
      expect(applyFrame(after, snapshot(0, 9))).toBe(after);
      expect(applyFrame(after, host)).toBe(after);
    }
    const badWire = applyFrame(ready(), { ...events(), v: 2 });
    expect(badWire.update).toBe(true);
    expect(badWire.stopped).toBe(true);
    expect(applyFrame(badWire, snapshot())).toBe(badWire);
  });

  it("greeting_fields_are_kept", () => {
    const greeting = { prompt: { kind: "trust" }, notices: ["retry"], root_note: "no boundary",
      shell_running: true, send_block: "bypass", theme };
    const m = applyFrame(applyFrame(initialTranscript(), host), { v: 1, kind: "greeting", key, greeting });
    expect(m).toMatchObject({ key, prompt: greeting.prompt, notices: ["retry"], rootNote: "no boundary",
      shellRunning: true, sendBlock: "bypass", device: "Pixel", location: "desk" });
  });

  it("gone_and_ended_are_kept", () => {
    let m = applyFrame(ready(), { v: 1, kind: "ended", key, how: "lost" });
    expect(m.ended).toBe("lost");
    m = applyFrame(m, { v: 1, kind: "gone", key, why: "closed" });
    expect(m.gone).toBe(true);
  });

  it("theme_from_greeting_and_frame", () => {
    const greeting = { theme, prompt: null, notices: [], root_note: null, shell_running: false, send_block: null };
    let m = applyFrame(initialTranscript(), { v: 1, kind: "greeting", key, greeting });
    expect(m.theme).toEqual(theme);
    const light = { ...theme, scheme: "light" };
    m = applyFrame(m, { v: 1, kind: "theme", theme: light });
    expect(m.theme).toEqual(light);
    expect(applyFrame(m, { v: 1, kind: "send_block", key, block: "watch_only" }).sendBlock).toBe("watch_only");
  });

  it("want_more_only_when_none_owed", () => {
    const before = ready();
    expect(wantMore(initialTranscript())).toBeNull();
    expect(wantMore(before)).toEqual({ kind: "more", key, generation: 1, before_turn: 2 });
    expect(before.owedPage).toBe(false);
    const owing = { ...before, owedPage: true };
    expect(wantMore(owing)).toBeNull();
    expect(applyFrame(owing, page()).owedPage).toBe(false);
    expect(wantMore(applyFrame(owing, page()))).toBeNull();
    expect(wantMore(before, 3)?.before_turn).toBe(3);
    expect(wantMore({ ...before, wantSnapshot: true })).toBeNull();
    expect(wantMore(applyFrame(before, { v: 1, kind: "refused", key, why: "no such turn" }))).not.toBeNull();
  });

  it("every_fixture_applies_without_error", () => {
    expect(Object.keys(fixtures).length).toBeGreaterThan(0);
    for (const frames of Object.values(fixtures)) {
      let m = initialTranscript();
      for (const frame of frames) {
        expect((frame as { kind: string }).kind).not.toBe("chunk");
        m = applyFrame(m, frame);
        expect(m.update).toBe(false);
        expect(m.wantSnapshot).toBe(false);
      }
    }
    for (const invalid of [null, [], 1, "text", {}, { kind: "events" }]) {
      expect(() => applyFrame(initialTranscript(), invalid)).not.toThrow();
    }
  });

  it("real_pages_restore_the_whole_entry_and_keep_live_state", () => {
    const frames = fixtures["../fixtures/phone-frames/history.json"] as { kind: string }[];
    expect(frames).toHaveLength(6);
    const tail = applyFrame(initialTranscript(), frames[0]);
    const older = applyFrame(tail, frames[1]);
    const duplicate = applyFrame(older, frames[2]);
    expect(duplicate.state).toEqual(older.state);
    expect(duplicate.cut).toHaveLength(1);
    const whole = applyFrame(duplicate, frames[3]);
    expect(whole.cut).toEqual([]);
    expect(whole.firstTurn).toBe(0);
    expect(whole.state.transcript.map((entry) => entry.seq)).toEqual([1, 4, 7]);
    expect(whole.state.transcript[1].text.length).toBeGreaterThan(100_000);
    expect(whole.state.nextSeq).toBe(tail.state.nextSeq);
    expect(whole.state.status).toEqual(tail.state.status);
    const fresh = applyFrame(whole, frames[4]);
    expect(fresh.generation).toBe(2);
    expect(applyFrame(fresh, frames[5])).toBe(fresh);
    const live = (fixtures["../fixtures/phone-frames/live.json"] as unknown[])
      .reduce<ReturnType<typeof initialTranscript>>(applyFrame, initialTranscript());
    expect(live.sendBlock).toBe("watch_only");
    expect(live.gone).toBe(true);
    expect(live.ended).toBe("lost");
  });
});
