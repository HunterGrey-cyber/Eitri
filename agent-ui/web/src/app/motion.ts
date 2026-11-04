/** One `j`/`k` press's worth of scroll inside a tool result's own overflow box, chosen to read
 *  like the row-to-row step it stands in for rather than a full-page jump. Arbitrary and not
 *  verified on a screen. */
export const TOOL_RESULT_SCROLL_STEP_PX = 40;

/** The longest this panel swallows keys after `f` asked `shell` for a HINT, if that HINT never ends
 *  here (see `hintPendingRef`). Long enough for a GTK main loop busy with a frame and the 33ms pump; short
 *  enough that a `shell` which never answers (the HINT was refused, or this page is not hosted by
 *  `shell` at all) costs a second of dead keys, not a stuck panel -- the same stance as `shell`'s own
 *  300ms wait for this panel's answer. */
export const HINT_PENDING_TIMEOUT_MS = 1000;

/** The highest count a digit prefix (`3j`, `1000j`, …) accumulates to before a
 *  motion repeats it. Vim itself never errors on an absurd count -- it silently caps it (a headless
 *  `nvim --headless -u NONE -c "call feedkeys('99999999999999999999j', 'xt')" -c "echo v:count"`
 *  probe against this host's real nvim reports `999999999`, vim's own internal `long` overflow
 *  clamp, whatever the typed digit string). This panel's cap is far smaller and purpose-fit rather
 *  than matching that number: even repeating a cheap DOM lookup `MAX_MOTION_COUNT` times is already
 *  more than any real motion needs, and the cap exists only as a second bound underneath the
 *  boundary early-exit below (the loop stopping the moment a repeat makes no further progress) --
 *  belt and suspenders, not the primary fix. */
export const MAX_MOTION_COUNT = 9999;

/** Folds one more typed digit into a count-in-progress, capped at `MAX_MOTION_COUNT` on every step
 *  (not only at the end) so an arbitrarily long run of digits -- someone holding a number key --
 *  never builds a number past the cap even transiently. A pure function so the cap is testable
 *  without a DOM: see `onKeyDown`'s `action.kind === "count"` branch for the one call site. */
export function accumulateMotionCount(count: number | null, digit: number): number {
  return Math.min(MAX_MOTION_COUNT, (count ?? 0) * 10 + digit);
}

/** The row `gg` / `G` lands on. Bare, the first or the last row. With a count it is
 *  row N, counted from 1 and clamped to the rows there are -- vim's `{N}gg` and `{N}G` (`:help gg`,
 *  `:help G`), where a count past the end lands on the last line. A pure function so `onKeyDown`'s two
 *  readers of it (the scroll announcement and the jump itself) cannot disagree, and so the clamp is
 *  testable without a DOM. */
export function jumpTarget(to: "first" | "last", count: number | null, rows: number): number {
  const last = Math.max(rows - 1, 0);
  if (count === null) return to === "first" ? 0 : last;
  return Math.max(0, Math.min(count, rows) - 1);
}

