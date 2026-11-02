import { vi } from "vitest";

/* A test's fake timers do not move jsdom's animation frames, so a test that waits out the answer guard would wait for
   a paint that never comes. Here a commit counts as painted at once, on the clock the test controls. A test about the
   paint itself takes the real one back with `vi.unmock("./afterPaint")` and fakes `requestAnimationFrame`
   (`App.answerPaint.test.tsx`). */
vi.mock("./afterPaint", () => ({
  afterPaint: (run: () => void) => {
    run();
    return () => {};
  },
}));
