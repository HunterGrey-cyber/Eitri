// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render } from "@testing-library/react";

// See ModeSelector.test.tsx: `globals` is off, so RTL's automatic cleanup is not registered.
afterEach(cleanup);
import { TurnActivity } from "./TurnActivity";
import type { TurnPhase } from "../turnPhase";

describe("TurnActivity", () => {
  it("draws the meter for every phase except blocked, and never for blocked", () => {
    const phases: TurnPhase[] = [
      { kind: "sent" },
      { kind: "thinking" },
      { kind: "replying" },
      { kind: "tool", toolName: "Bash" },
    ];
    for (const phase of phases) {
      const { container, unmount } = render(<TurnActivity phase={phase} clock={null} />);
      expect(container.querySelector(".meter-fill")).not.toBeNull();
      unmount();
    }
    const { container } = render(<TurnActivity phase={{ kind: "blocked" }} clock={null} />);
    expect(container.querySelector(".meter-fill")).toBeNull();
    // The word still renders even with the meter withheld -- only the motion stops (design §2).
    expect(container.querySelector(".turn-state")?.textContent).toBe("waiting for you");
  });

  it("carries data-phase so the phase is inspectable without depending on the rendered word", () => {
    const { container } = render(<TurnActivity phase={{ kind: "tool", toolName: "Read" }} clock={null} />);
    expect(container.querySelector(".turn-activity")?.getAttribute("data-phase")).toBe("tool");
  });

  it("ticks the elapsed clock once a second, without re-mounting the meter", () => {
    vi.useFakeTimers();
    try {
      const since = Date.now();
      const { container } = render(<TurnActivity phase={{ kind: "sent" }} clock={{ turnId: "t1", since, exact: true }} />);
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("0s");
      const meterBeforeTick = container.querySelector(".meter-fill");

      act(() => {
        vi.advanceTimersByTime(1000);
      });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("1s");
      // The SAME meter element, not a new one -- a tick re-renders this leaf's text, not the whole
      // subtree, and the meter must never be torn down and rebuilt every second (§5.4's "no
      // will-change" argument assumes exactly one persistent element for the animation's lifetime).
      expect(container.querySelector(".meter-fill")).toBe(meterBeforeTick);

      act(() => {
        vi.advanceTimersByTime(4000);
      });
      expect(container.querySelector(".turn-elapsed")?.textContent).toBe("5s");
    } finally {
      vi.useRealTimers();
    }
  });

  it("clears its interval on unmount -- advancing timers afterwards updates nothing and warns of nothing", () => {
    vi.useFakeTimers();
    try {
      const clearSpy = vi.spyOn(window, "clearInterval");
      const warn = vi.spyOn(console, "error").mockImplementation(() => {});
      const { unmount } = render(<TurnActivity phase={{ kind: "sent" }} clock={{ turnId: "t1", since: Date.now(), exact: true }} />);
      unmount();
      expect(clearSpy).toHaveBeenCalled();

      // If the interval were still running, this would call `setState` on an unmounted component,
      // which React reports as a console error.
      act(() => {
        vi.advanceTimersByTime(5000);
      });
      expect(warn).not.toHaveBeenCalled();
      clearSpy.mockRestore();
      warn.mockRestore();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows no elapsed clock at all when clock is null, rather than guessing at one", () => {
    const { container } = render(<TurnActivity phase={{ kind: "sent" }} clock={null} />);
    expect(container.querySelector(".turn-elapsed")).toBeNull();
  });
});
