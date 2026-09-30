// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { applyEditorTyping, NATURAL_METER_STEP_MS, typingMeterStepMs } from "./typingCadence";

/**
 * Owner decision #37 (revised 2026-09-29): while the user types in the editor, the panel's stream
 * reaches the page on an even cadence (`core/src/panel_cadence.rs`), and the page's own motion --
 * the turn meter -- must not repaint faster than that, or it adds the frame-clock cycles the pacing
 * removes. It is slowed, never paused: the meter keeps moving.
 */
describe("typingMeterStepMs", () => {
  it("never speeds the meter up: the natural step is the floor", () => {
    expect(NATURAL_METER_STEP_MS).toBe(300);
    for (const period of [16, 125, 250, 299, 300]) {
      expect(typingMeterStepMs(period), `${period} ms period`).toBe(300);
    }
  });

  it("follows a cadence slower than the natural step, one repaint per push at most", () => {
    expect(typingMeterStepMs(333)).toBe(333);
    expect(typingMeterStepMs(500)).toBe(500);
    expect(typingMeterStepMs(1000)).toBe(1000);
  });

  it("falls back to the natural step for a period that is not a usable number", () => {
    for (const period of [0, -5, Number.NaN, Number.POSITIVE_INFINITY]) {
      expect(typingMeterStepMs(period), `${period}`).toBe(300);
    }
  });
});

describe("applyEditorTyping", () => {
  it("sets the attribute and the meter step while typing, and removes both after", () => {
    const root = document.createElement("div");
    applyEditorTyping(root, true, 500);
    expect(root.hasAttribute("data-editor-typing")).toBe(true);
    expect(root.style.getPropertyValue("--meter-step")).toBe("500ms");
    applyEditorTyping(root, false, 500);
    expect(root.hasAttribute("data-editor-typing")).toBe(false);
    expect(root.style.getPropertyValue("--meter-step")).toBe("");
  });

  it("keeps a period faster than the natural step at the natural step", () => {
    const root = document.createElement("div");
    applyEditorTyping(root, true, 125);
    expect(root.style.getPropertyValue("--meter-step")).toBe("300ms");
  });

  it("treats a period it cannot use as not typing, so nothing is left set by a malformed message", () => {
    const root = document.createElement("div");
    applyEditorTyping(root, true, 500);
    applyEditorTyping(root, true, Number.NaN);
    expect(root.hasAttribute("data-editor-typing")).toBe(false);
    expect(root.style.getPropertyValue("--meter-step")).toBe("");
    applyEditorTyping(root, true, 0);
    expect(root.hasAttribute("data-editor-typing")).toBe(false);
  });

  it("is idempotent in both directions", () => {
    const root = document.createElement("div");
    applyEditorTyping(root, false, 500);
    applyEditorTyping(root, false, 500);
    expect(root.hasAttribute("data-editor-typing")).toBe(false);
    applyEditorTyping(root, true, 500);
    applyEditorTyping(root, true, 1000);
    expect(root.style.getPropertyValue("--meter-step")).toBe("1000ms");
    applyEditorTyping(root, false, 0);
    expect(root.hasAttribute("data-editor-typing")).toBe(false);
  });
});
