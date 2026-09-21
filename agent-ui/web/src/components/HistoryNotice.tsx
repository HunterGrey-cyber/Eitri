import type { HistoryNotice as HistoryNoticeData } from "../types";
import { historyNoticeText } from "../history";

/**
 * A fixed notice at the head of the conversation says which history seeded the session.
 * Show it for complete and truncated restores so earlier messages are distinct from new ones.
 * The source matters because the original transcript and the local session copy can differ.
 *
 * This is not a conversation row: `data-nav-stop="row"` would shift every row index below it,
 * including the cursor, `permissionTarget`, and scroll helpers. It has no `seq` and never enters
 * `buildTimeline`, so it does not affect timeline ordering.
 *
 * `stopsIn` keeps non-row stops only when they contain an enabled control. The Copy path button
 * makes this notice reachable by `j`/`k` and gives access to its source file. `sourcePath` is
 * nonempty because restored history comes from one file, so the button is always present.
 * The notice has no Dismiss control because it must remain visible whenever history was restored.
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
