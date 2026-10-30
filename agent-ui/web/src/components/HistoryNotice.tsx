import type { HistoryNotice as HistoryNoticeData } from "../types";
import { historyNoticeText } from "../history";
import { copyToPrimary } from "../primaryCopy";

/** The restored-history source, shown even without truncation so past and live text are distinct.
 *  Claude's transcript and Eitri's copy can differ. At a 400-item cap, only 8 of 44 measured sessions
 *  truncate, so a truncation-only notice would usually be absent.
 *
 *  Never a conversation row: it has no `seq` or timeline item, and counting it would shift every
 *  cursor index, permission target and scroll helper below it.
 *
 *  Copy path both opens a route to the source file and makes the notice a j/k stop: `stopsIn`
 *  filters out non-row stops without enabled controls. `sourcePath` is non-empty because restored
 *  history comes from one file, so the button always exists. No Dismiss: the source must stay shown.
 */
export function HistoryNotice({ notice }: { notice: HistoryNoticeData }) {
  return (
    <div className="history-notice" data-nav-stop="notice">
      <span>{historyNoticeText(notice)}</span>
      {/* `title` as well as the clipboard: the path is long and absolute, so it is reachable on
          hover without being pasted into the sentence and wrapping a narrow panel several times. */}
      <button title={notice.sourcePath} onClick={() => {
        void navigator.clipboard?.writeText(notice.sourcePath);
        copyToPrimary(notice.sourcePath);
      }}>
        Copy path
      </button>
    </div>
  );
}
