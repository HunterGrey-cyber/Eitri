// @vitest-environment jsdom
import { render } from "@testing-library/react";
import { expect, it } from "vitest";
import { SyncChip } from "./syncChip";
import { initialPageModel, pageModel } from "./page_model";

const initial = { host: "example-host", phase: "watching", gitPct: 0, conflicts: 0, pausedReason: null };
it("sync_chip_shows_each_phase", () => {
  const { container, rerender } = render(<SyncChip chip={initial} />);
  for (const [phase, suffix] of [["watching", ""], ["syncing", " · syncing"], ["history", " · history 43 %"], ["paused", " · paused"]]) {
    const chip = { ...initial, phase, gitPct: 43 };
    const model = pageModel(initialPageModel(), {kind: "sync_status", chip, receivedAt: 0});
    rerender(<SyncChip chip={model.syncChip!} />);
    expect(container.textContent).toBe("⇅ example-host" + suffix);
  }
});
it("conflict_chip_uses_the_error_colour", () => {
  const { getByText } = render(<SyncChip chip={{ ...initial, conflicts: 2 }} />);
  expect(getByText("⇅ example-host · 2 conflicts").getAttribute("style")).toContain("var(--nv-error)");
});
