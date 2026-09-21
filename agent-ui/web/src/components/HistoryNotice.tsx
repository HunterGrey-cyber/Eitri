import type { HistoryNotice as HistoryNoticeData } from "../types";
import { historyNoticeText } from "../history";

/**
 * One fixed line at the top of the conversation saying what this session was seeded with
 * (design §5.5, §10's first change).
 *
 * **Drawn whenever history was restored, not only when it was truncated.** The question it answers
 * first is "is this what we said last time, or what we said just now?", and the second is "where
 * did it come from" -- which has consequences (§8: Claude's own transcript and Neovibe's copy of
 * the same session can genuinely differ). Truncation is the third thing it can say, and since the
 * item cap became 400 it is the rare one: 8 of this machine's 44 real sessions.
 *
 * **It is not a conversation row and must never become one.** Row indices are counted across the
 * whole panel (`App.tsx`), so a `data-nav-stop="row"` here would shift the cursor index of every
 * row below it and carry `permissionTarget` and the scroll helpers with it. It holds no `seq`,
 * never reaches `buildTimeline`, and takes no part in the timeline's ordering.
 *
 * **The Copy path button is what makes the row reachable at all.** `nav.ts`'s `stopsIn` keeps a
 * stop only when it is a row or contains an enabled control, so a notice that merely copied
 * `.command-notice`'s `data-nav-stop` attribute would be silently filtered out of `j`/`k` -- which
 * is exactly what the first draft of this row did. The button is not there to satisfy that rule,
 * though: §5.5 wants a way out to the file itself, and `y`-to-copy is this panel's own precedent
 * (the terminal-handoff command). `sourcePath` is never empty -- a notice exists only when history
 * was restored, and history comes from exactly one file -- so the control always exists and the
 * row is always reachable.
 *
 * **No Dismiss**, unlike `.command-notice`: a line that can be closed does not satisfy "whenever
 * history was restored, this is shown" (invariant 11).
 */
export function HistoryNotice({ notice }: { notice: HistoryNoticeData }) {
  return (
    <div className="history-notice" data-nav-stop="notice">
      <span>{historyNoticeText(notice)}</span>
      {/* `title` as well as the clipboard: the path is long and absolute, so it is reachable on
          hover without being pasted into the sentence and wrapping a narrow panel several times. */}
      <button title={notice.sourcePath} onClick={() => void navigator.clipboard?.writeText(notice.sourcePath)}>
        Copy path
      </button>
    </div>
  );
}
