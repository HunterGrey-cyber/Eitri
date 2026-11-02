/** Runs `run` once what the last commit drew is on the screen, and returns how to call that off.
 *
 *  The first animation frame runs before the frame that shows the commit is painted; the second runs after it.
 *  A view that is not shown gets no frames, so `run` waits until it is shown: a question nobody can see has not
 *  been on screen for any length of time. */
export function afterPaint(run: () => void): () => void {
  let frame = requestAnimationFrame(() => {
    frame = requestAnimationFrame(() => run());
  });
  return () => cancelAnimationFrame(frame);
}
