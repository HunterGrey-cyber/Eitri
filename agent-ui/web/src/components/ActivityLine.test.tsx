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

it("moves its star while the agent works, and holds it still while a card waits or the phase is the user's", () => {
  const onInterrupt = vi.fn();
  const glyph = (c: HTMLElement) => c.querySelector<HTMLElement>(".activity-glyph")!;
  const { container, rerender } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} />,
  );
  expect(glyph(container).textContent?.trim()).toBe("✻");
  expect(glyph(container).getAttribute("aria-hidden")).toBe("true");
  expect(glyph(container).dataset.moving).toBe("true");
  const moving = glyph(container);
  // The same element across a re-render: the animation is never torn down and restarted by one.
  rerender(<ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={1} />);
  expect(glyph(container)).toBe(moving);
  // A card waiting: the agent is not working, so the star does not say it is.
  rerender(<ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} pendingTool="Bash" />);
  expect(glyph(container).dataset.moving).toBeUndefined();
  // No meter: the star is the line's only motion.
  expect(container.querySelector(".meter, .meter-fill")).toBeNull();
});

it("counts the queue, names the interrupt key, and names a card that waits (P1)", () => {
  const onInterrupt = vi.fn();
  const { container, rerender } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={2} pendingTool={null} />,
  );
  // The interrupt words ARE the Stop button now, lowercase
  // like Claude Code's own bin string -- not a separate span beside a "Stop" button.
  expect(container.querySelector(".activity-line")!.textContent).toContain("2 queued · ctrl+c interrupt");
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
  rerender(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} queued={0} pendingTool="Bash" mode="input" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — Esc, j to it, a / d alone");
  expect(container.textContent).not.toContain("queued");
});

/** The card row reads differently by mode -- BROWSE can
 *  answer `a`/`d` directly, INPUT's keys go to the composer so it says `Esc` first. Defaults to
 *  BROWSE for every caller that predates this (`mode?`'s own doc comment). */
it("the card row says a / d alone in BROWSE, and defaults to it", () => {
  const onInterrupt = vi.fn();
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={onInterrupt} pendingTool="Bash" />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ Bash needs approval — j to it, a / d alone");
});

/** With a card waiting the activity line must keep the approval keys visible. The old
 *  wording drew the phase word and the card side by side, and at
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

/** The old "a / d" /
 *  "Esc, then a / d" said neither that the cursor check narrows `a`/`d` to the card under the cursor (never
 *  "the only card, from any row") nor that the typing guard makes them wait `TYPING_GUARD_MS` since the last key.
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

/** "the activity line already ends in `ctrl+c interrupt` ...; it is kept and
 *  pinned by a test, including while a card waits." Pins the button's own text, not just its
 *  presence -- CC's own words, unchanged by a card taking over the rest of the line. */
it("keeps ctrl+c interrupt as the Stop button's own text, even while a card waits (R34)", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool="Bash" />,
  );
  expect(container.querySelector("button.stop")!.textContent).toBe("ctrl+c interrupt");
});

/** The tool name is the agent's text (an MCP server names its own tools): drawn raw, a direction override
 *  in it reorders the words after it on the line that says which card waits. */
it("draws a hidden character in the waiting card's tool name as an escape", () => {
  const { container } = render(
    <ActivityLine state={running({ activeTurnId: "t1" })} turnClock={null} canInterrupt onInterrupt={vi.fn()} pendingTool={"mcp__x__‮evil"} />,
  );
  expect(container.querySelector(".activity-card")!.textContent).toBe("⚑ mcp__x__⟨U+202E⟩evil needs approval — j to it, a / d alone");
});
