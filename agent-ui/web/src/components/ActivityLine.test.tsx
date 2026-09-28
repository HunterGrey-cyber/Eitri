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
  // Panel round 2 (plan Task 10; spec §5.1): the interrupt words ARE the Stop button now, lowercase
  // like Claude Code's own bin string -- not a separate span beside a "Stop" button.
  expect(container.querySelector(".activity-line")!.textContent).toContain("2 queued · ctrl+c interrupt");
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
  rerender(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={0} pendingTool="Bash" mode="input" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — Esc, j to it, a / d alone");
  expect(container.textContent).not.toContain("queued");
});

/** Panel round 2 (plan Task 10; spec §5.1): the card row reads differently by mode -- BROWSE can
 *  answer `a`/`d` directly, INPUT's keys go to the composer so it says `Esc` first. Defaults to
 *  BROWSE for every caller that predates this (`mode?`'s own doc comment). */
it("the card row says a / d alone in BROWSE, and defaults to it", () => {
  const onInterrupt = vi.fn();
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} pendingTool="Bash" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — j to it, a / d alone");
});

/** Spec §5.1: "With a card waiting it reads `⚑ Bash needs approval — a / d`" (pre-hardening
 *  wording). The r2-gui GUI pass (2026-09-26) saw the phase word and the card side by side, and at
 *  520px both were cut to `waiti… ⚑ Write needs …` -- the keys that answer the card were the part
 *  lost. */
it("with a card waiting, the card is the line: no phase word or clock beside it", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Write" />,
  );
  expect(container.querySelector(".turn-activity")).toBeNull();
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Write needs approval — j to it, a / d alone");
  expect(container.querySelector("button.stop")).not.toBeNull();
});

/** v1 hardening, codex-release-p1 #9 (= R2-10 + R2-3, v1-release-review.md): the old "a / d" /
 *  "Esc, then a / d" said neither that S4 narrowed `a`/`d` to the card under the cursor (never
 *  "the only card, from any row") nor that S1 makes them wait `TYPING_GUARD_MS` since the last key.
 *  A user who moved off the card with `k`, or pressed `a` a beat after `Esc`, followed a hint that
 *  did not explain either refusal. */
it("codex-release-p1 #9: the hint names both S4's navigation and S1's pause, in both modes", () => {
  const { container, rerender } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Bash" />,
  );
  const text = () => container.querySelector(".activity-card")!.textContent!;
  expect(text()).toContain("j to it");
  expect(text()).toContain("alone");
  rerender(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Bash" mode="input" />,
  );
  expect(text()).toContain("Esc");
  expect(text()).toContain("j to it");
  expect(text()).toContain("alone");
});

/** R34 (spec §4.2): "the activity line already ends in `ctrl+c interrupt` ...; it is kept and
 *  pinned by a test, including while a card waits." Pins the button's own text, not just its
 *  presence -- CC's own words, unchanged by a card taking over the rest of the line. */
it("keeps ctrl+c interrupt as the Stop button's own text, even while a card waits (R34)", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Bash" />,
  );
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
});
