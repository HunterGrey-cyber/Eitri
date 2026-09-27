// @vitest-environment jsdom
import { afterEach, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render } from "@testing-library/react";
import { ActivityLine } from "./ActivityLine";
import { initialState } from "../reducer";
import type { AgentUiState } from "../types";

afterEach(cleanup);
const running = (over: Partial<AgentUiState>): AgentUiState => ({ ...initialState(), status: { kind: "running" }, ...over });

it("is absent while no turn runs, and after the session ended whatever its last fields say", () => {
  const onInterrupt = vi.fn();
  const idle = render(<ActivityLine state={running({ activeTurnId: null })} turnClock={null} canInterrupt onInterrupt={onInterrupt} />);
  expect(idle.container.querySelector(".activity-line")).toBeNull();
  idle.unmount();
  const closed = render(
    <ActivityLine state={{ ...running({ activeTurnId: "t1" }), status: { kind: "closed", reason: "x" } }} turnClock={null} canInterrupt onInterrupt={onInterrupt} />,
  );
  expect(closed.container.querySelector(".activity-line")).toBeNull();
});

it("shows the turn's motion and Stop while working, and Stop interrupts", () => {
  const onInterrupt = vi.fn();
  const { container, rerender } = render(<ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} />);
  const line = container.querySelector('.activity-line[data-nav-stop="status"]')!;
  fireEvent.click(line.querySelector("button.stop")!);
  expect(onInterrupt).toHaveBeenCalledTimes(1);
  rerender(<ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt={false} onInterrupt={onInterrupt} />);
  expect(container.querySelector("button.stop")).toBeNull();
});

it("counts the queue, names the interrupt key, and names a card that waits (P1)", () => {
  const onInterrupt = vi.fn();
  const { container, rerender } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={2} pendingTool={null} />,
  );
  // The lowercase interrupt hint is the Stop button's label, so the hint itself is clickable.
  expect(container.querySelector(".activity-line")!.textContent).toContain("2 queued · ctrl+c interrupt");
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
  rerender(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={0} pendingTool="Bash" mode="input" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — Esc, then a / d");
  expect(container.textContent).not.toContain("queued");
});

/** The card hint names `a`/`d` in BROWSE. In INPUT it says Escape first because keys go to the
 *  composer. Callers that omit the mode get BROWSE wording. */
it("the card row says a / d alone in BROWSE, and defaults to it", () => {
  const onInterrupt = vi.fn();
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} pendingTool="Bash" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — a / d");
});

/** A waiting card gets the activity line without a phase word beside it.
 *  At 520px, showing both truncated the card's answer keys. */
it("with a card waiting, the card is the line: no phase word or clock beside it", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Write" />,
  );
  expect(container.querySelector(".turn-activity")).toBeNull();
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Write needs approval — a / d");
  expect(container.querySelector("button.stop")).not.toBeNull();
});

/** "the activity line already ends in `ctrl+c interrupt` ...; it is kept and
 *  pinned by a test, including while a card waits." Pins the button's own text, not just its
 *  presence -- CC's own words, unchanged by a card taking over the rest of the line. */
it("keeps ctrl+c interrupt as the Stop button's own text, even while a card waits (R34)", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Bash" />,
  );
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
});
