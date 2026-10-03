import { forwardRef, useEffect } from "react";
import type { KeyboardEvent } from "react";
import { Row } from "./Row";
import { SearchBar } from "./SearchBar";
import {
  REVIEW_KEY_LINE,
  clock,
  commentsUnder,
  counts,
  cursorIndex,
  draftLine,
  fileFlags,
  hasPatch,
  hidesFiles,
  originSign,
  promptText,
  revertMark,
  revertOf,
  reviewNotes,
  reviewStops,
  reviewTitle,
  splitFiles,
  stopKey,
  tooLargeNote,
} from "../review";
import type { ReviewState } from "../review";
import type { ReviewFile, ReviewHunk, ReviewLine, ReviewNotOnDisk } from "../types";

type Props = {
  /** What to draw; `App.tsx` owns it, and owns every key while the overlay is open. */
  state: ReviewState;
  /** A click on the backdrop. `q`, `Escape` and `c` close it from the keyboard, in `App.tsx`. */
  onClose: () => void;
  /** The comment input's text, Enter and Escape. The input is a real text box that stops its own Enter and
   *  Escape (as the `:` line's does), so `App.tsx` hears them here and not as keys. Absent in a test that only
   *  draws. */
  onCommentChange?: (text: string) => void;
  onCommentAccept?: (event: KeyboardEvent<HTMLInputElement>) => void;
  onCommentCancel?: (event: KeyboardEvent<HTMLInputElement>) => void;
};

/** The `.diff-*` class a patch line takes. A "no newline at end of file" marker is a note on the line
 *  above it, so it draws as context. */
function lineClass(line: ReviewLine): string {
  return line.kind === "added" ? "diff-added" : line.kind === "removed" ? "diff-removed" : "diff-context";
}

function lineGutter(line: ReviewLine): string {
  return line.kind === "added" ? "+" : line.kind === "removed" ? "-" : line.kind === "no_newline" ? "\\" : " ";
}

/** A comment's lines as a person says them: `837`, or `837-842`. */
function span(from: number, to: number): string {
  return from === to ? `${from}` : `${from}-${to}`;
}

/** A revert the send leaves out: a hunk by its lines, a whole-file revert by what it did to the file. */
function notOnDiskLine(entry: ReviewNotOnDisk): string {
  if (entry.lines !== null) {
    const [from, to] = entry.lines;
    return `${entry.path} ${from === to ? "line" : "lines"} ${span(from, to)}: ${entry.why}`;
  }
  const what = entry.what === "deleted" ? "deleted" : entry.what === "restored" ? "restored" : "whole file";
  return `${entry.path} (${what}): ${entry.why}`;
}

function Hunk({ hunk, path, state, cursor }: { hunk: ReviewHunk; path: string; state: ReviewState; cursor: string | null }) {
  const mark = revertMark(revertOf(state, path, hunk));
  return (
    <div className="review-hunk" data-path={path}>
      <Row kind="review-hunk" sign="▾" current={cursor === stopKey({ kind: "hunk", path, id: hunk.id })} navStop="hunk">
        <span className="review-hunk-header">{hunk.header}</span>
        {mark !== null && <span className="review-file-flag">{mark}</span>}
      </Row>
      <div className="review-hunk-lines">
        {hunk.lines.map((line, i) => (
          <div key={i} className={`diff-line ${lineClass(line)}`}>
            <span className="review-lineno">{line.oldNo ?? ""}</span>
            <span className="review-lineno">{line.newNo ?? ""}</span>
            <span className="diff-gutter" aria-hidden="true">
              {lineGutter(line)}
            </span>
            <span className="diff-text">{line.text}</span>
          </div>
        ))}
      </div>
      {commentsUnder(state, path, hunk).map((comment) => (
        <Row key={comment.id} kind="review-comment" sign="»" current={cursor === stopKey({ kind: "comment", path, id: comment.id })} navStop="comment">
          <span className="review-comment-lines">{span(comment.from, comment.to)}</span>
          <span className="review-comment-text">{comment.text}</span>
        </Row>
      ))}
    </div>
  );
}

/** One file: its row and, when open, whatever its patch has got to. The hunks are separate stops under the
 *  file's own, so the cursor can walk them. */
function FileEntry({ file, state, cursor }: { file: ReviewFile; state: ReviewState; cursor: string | null }) {
  const open = state.expanded.includes(file.path);
  const entry = state.diffs.get(file.path);
  const flags = fileFlags(file);
  const mark = revertMark(revertOf(state, file.path, null));
  return (
    <div className="review-file" data-path={file.path}>
      <Row kind="review-file" sign={originSign(file)} current={cursor === stopKey({ kind: "file", path: file.path })} navStop="file">
        <span className="review-path">{file.path}</span>
        <span className="review-counts">{counts(file.added, file.removed)}</span>
        {flags.map((flag) => (
          <span key={flag} className="review-file-flag">
            {flag}
          </span>
        ))}
        {mark !== null && <span className="review-file-flag">{mark}</span>}
      </Row>
      {open && hasPatch(file) && (
        <div className="review-patch">
          {entry === undefined || entry.status === "loading" ? (
            <div className="review-note">loading…</div>
          ) : entry.status === "failed" ? (
            <div className="review-note">could not load this file's changes: {entry.error}</div>
          ) : entry.diff.hunks === null ? (
            <div className="review-note">{tooLargeNote(entry.diff.added, entry.diff.removed)}</div>
          ) : entry.diff.hunks.length === 0 ? (
            <div className="review-note">no changes to show as text</div>
          ) : (
            entry.diff.hunks.map((hunk) => <Hunk key={hunk.id} hunk={hunk} path={file.path} state={state} cursor={cursor} />)
          )}
        </div>
      )}
    </div>
  );
}

/**
 * The turn review overlay (`c` in BROWSE): the files that changed on disk during one turn, or since the
 * session's first baseline, and each file's hunks on `Enter`. Read-only. It draws only; `App.tsx` decides
 * when it is open, swallows every key while it is (so `a`/`d` cannot reach a card hidden underneath) and
 * keeps the one `ReviewState` it reads.
 *
 * It sits over `.agent-ui-scroller`, like the `?` overlay, so the winbar and the status band stay visible.
 * Its rows reuse `Row` (the sign column and the cursor block come with it) and its patches the `.diff-*`
 * classes, with no new colour: nothing here syntax-highlights, and nothing is truncated -- a patch the
 * shell would not send is refused with its counts. The words never say who changed a file: the header
 * says "changed on disk during this turn", and the signs carry the attribution.
 */
export const ReviewOverlay = forwardRef<HTMLDivElement, Props>(function ReviewOverlay({ state, onClose, onCommentChange, onCommentAccept, onCommentCancel }, ref) {
  const stops = reviewStops(state);
  const at = cursorIndex(state, stops);
  const cursor = stops[at] === undefined ? null : stopKey(stops[at]);
  const envelope = state.envelope;

  // The cursor stays on screen as it moves: `?.()` because jsdom has no `scrollIntoView`.
  useEffect(() => {
    if (cursor === null) return;
    const root = typeof ref === "object" && ref !== null ? ref.current : null;
    root?.querySelector<HTMLElement>(".row-current")?.scrollIntoView?.({ block: "nearest" });
  }, [cursor, ref]);

  const loading = envelope === null ? state.error === null : state.requestId !== envelope.requestId && state.error === null;
  const { named, unnamed } = envelope === null ? { named: [], unnamed: [] } : splitFiles(envelope);

  return (
    <div
      className="review-overlay"
      role="dialog"
      aria-label="Review"
      ref={ref}
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <header className="review-header">
        <div className="review-title">{envelope === null ? "review" : reviewTitle(envelope)}</div>
        {loading && <div className="review-flag">loading…</div>}
        {state.error !== null && <div className="review-flag">{state.error}</div>}
        {envelope !== null &&
          reviewNotes(envelope).map((note, i) => (
            <div key={i} className="review-flag">
              {note}
            </div>
          ))}
      </header>
      {state.recovery.length > 0 && (
        <div className="review-recovery">
          {state.recovery.map((entry) => (
            <Row key={entry.id} kind="review-recovery" sign="!" current={cursor === stopKey({ kind: "recovery", id: entry.id })} navStop="recovery">
              <span className="review-path">an interrupted revert left {entry.path}</span>
              <span className="review-file-flag">{clock(entry.at)}</span>
            </Row>
          ))}
        </div>
      )}
      {envelope !== null && !hidesFiles(envelope) && (
        <div className="review-files">
          {named.length === 0 && unnamed.length === 0 && <div className="review-note">no files changed on disk</div>}
          {named.map((file) => (
            <FileEntry key={file.path} file={file} state={state} cursor={cursor} />
          ))}
          {unnamed.length > 0 && (
            <>
              <Row kind="review-group" sign={state.groupOpen ? "▾" : "▸"} current={cursor === "group"} navStop="group">
                <span className="review-group-title">changed outside this tab's edits ({unnamed.length})</span>
              </Row>
              {state.groupOpen && unnamed.map((file) => <FileEntry key={file.path} file={file} state={state} cursor={cursor} />)}
            </>
          )}
          {envelope.pendingNoResult > 0 && (
            <div className="review-note">
              {envelope.pendingNoResult} file-changing {envelope.pendingNoResult === 1 ? "call has" : "calls have"} no result (still running, or refused)
            </div>
          )}
        </div>
      )}
      {draftLine(state.draft) !== null && <div className="review-draft">{draftLine(state.draft)}</div>}
      {state.prompt !== null && (
        <div className="review-prompt">
          {state.prompt.kind === "send" && state.prompt.preview !== null && (
            <div className="review-preview">
              <div className="review-preview-text">{state.prompt.preview.text}</div>
              {state.prompt.preview.notOnDisk.map((entry) => (
                <div key={entry.id} className="review-note">
                  {notOnDiskLine(entry)}
                </div>
              ))}
            </div>
          )}
          <div className="review-question">{promptText(state.prompt)}</div>
          {state.prompt.kind === "comment" && (
            <SearchBar
              lead="comment: "
              label="Comment"
              query={state.prompt.text}
              onChange={(text) => onCommentChange?.(text)}
              onAccept={(event) => onCommentAccept?.(event)}
              onCancel={(event) => onCommentCancel?.(event)}
            />
          )}
        </div>
      )}
      {state.status !== null && <div className="review-status">{state.status}</div>}
      <footer className="review-keys">{REVIEW_KEY_LINE}</footer>
    </div>
  );
});
