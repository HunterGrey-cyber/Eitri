import { describe, expect, it } from "vitest";
import { decodeState, hasPosition, MAX_STATE_BYTES, onRestored, onSave, type MarkdownState } from "./handoff";

const good: MarkdownState = { v: 1, topLine: 120, fraction: 0.25, search: "needle" };

describe("handoff", () => {
  it("decodes a state and refuses anything else", () => {
    expect(decodeState(good)).toEqual(good);
    expect(decodeState({ ...good, search: null })).toEqual({ ...good, search: null });
    for (const bad of [null, [], 3, "x", { ...good, v: 2 }, { ...good, topLine: -1 }, { ...good, topLine: 1.5 },
      { ...good, fraction: 1 }, { ...good, fraction: Number.NaN }, { ...good, search: 4 },
      { ...good, noDocument: false }, { ...good, noDocument: "yes" }]) {
      expect(decodeState(bad)).toBeNull();
    }
  });

  it("keeps a state without a document, which has no position to wait for", () => {
    const empty: MarkdownState = { v: 1, topLine: 0, fraction: 0, search: "draft", noDocument: true };
    expect(decodeState(empty)).toEqual(empty);
    expect(hasPosition(decodeState(empty)!)).toBe(false);
    expect(hasPosition(decodeState(good)!)).toBe(true);
    expect(decodeState(good)).not.toHaveProperty("noDocument");
  });

  it("answers host.save with the state, or no state over the cap", () => {
    const sent: [string, unknown][] = [];
    onSave(() => good, (verb, payload) => sent.push([verb, payload]));
    expect(sent).toEqual([["page.saved", { state: good }]]);
    sent.length = 0;
    onSave(() => ({ ...good, search: "x".repeat(MAX_STATE_BYTES) }), (verb, payload) => sent.push([verb, payload]));
    expect(sent).toEqual([["page.saved", { state: null }]]);
  });

  it("applies a restored state, then says ready one frame later", () => {
    const order: string[] = [];
    let frame: (() => void) | null = null;
    onRestored(good, () => order.push("apply"), (cb) => { frame = cb; }, (verb) => order.push(verb));
    expect(order).toEqual(["apply"]);
    frame!();
    expect(order).toEqual(["apply", "page.ready"]);
  });

  it("says ready without applying when nothing usable was restored", () => {
    const order: string[] = [];
    onRestored({ v: 9 }, () => order.push("apply"), (cb) => cb(), (verb) => order.push(verb));
    expect(order).toEqual(["page.ready"]);
  });
});
