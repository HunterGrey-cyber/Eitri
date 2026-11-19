import { describe, expect, it } from "vitest";
import { ARMED_FOR_MS, CompositionGuard, registerImeCancel } from "./imeCancel";

const snap = { text: "draft", selectionStart: 5, selectionEnd: 5 };

describe("CompositionGuard", () => {
  it("puts the text back when the host's cancel was armed for the composition that ends", () => {
    let t = 0;
    const g = new CompositionGuard(() => t);
    g.start(snap);
    expect(g.arm()).toBe(true);
    t = 10;
    expect(g.end("draftni hao")).toEqual(snap);
  });

  it("leaves a draft something else rewrote during the composition alone", () => {
    const g = new CompositionGuard(() => 0);
    g.start({ text: "keep this", selectionStart: 4, selectionEnd: 4 });
    g.arm();
    expect(g.end("an unsent message given back")).toBeNull();
    g.start({ text: "keep this", selectionStart: 4, selectionEnd: 4 });
    g.arm();
    expect(g.end("keepni hao this")).toEqual({ text: "keep this", selectionStart: 4, selectionEnd: 4 });
  });

  it("is disarmed when the host's move was refused", () => {
    const g = new CompositionGuard(() => 0);
    g.start(snap);
    g.arm();
    g.disarm();
    expect(g.end("draft你好")).toBeNull();
  });

  it("leaves a composition the person commits alone", () => {
    const g = new CompositionGuard(() => 0);
    g.start(snap);
    expect(g.end("draft你好")).toBeNull();
  });

  it("arms nothing with no composition open", () => {
    const g = new CompositionGuard(() => 0);
    expect(g.arm()).toBe(false);
    expect(g.end("draft")).toBeNull();
  });

  it("drops an armed cancel at the person's next key, a new composition, or after its time", () => {
    let t = 0;
    const g = new CompositionGuard(() => t);
    g.start(snap);
    g.arm();
    g.key();
    expect(g.end("draftni hao")).toBeNull();

    g.start(snap);
    g.arm();
    g.start({ ...snap, text: "draft2" });
    expect(g.end("draft2ni hao")).toBeNull();

    g.start(snap);
    g.arm();
    t = ARMED_FOR_MS + 1;
    expect(g.end("draftni hao")).toBeNull();
  });
});

describe("registerImeCancel", () => {
  it("answers armed when a registered composer had a composition open, and stops answering once unregistered", () => {
    const w = {} as Window;
    let open = true;
    let disarmed = 0;
    const unregister = registerImeCancel({ arm: () => open, disarm: () => (disarmed += 1) }, w);
    expect(w.__eitriImeCancel?.()).toBe("armed");
    expect(w.__eitriImeCancel?.(false)).toBe("disarmed");
    expect(disarmed).toBe(1);
    open = false;
    expect(w.__eitriImeCancel?.()).toBe("idle");
    unregister();
    open = true;
    expect(w.__eitriImeCancel?.()).toBe("idle");
  });
});
