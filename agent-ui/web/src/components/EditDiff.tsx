import { hunksOf } from "../hunks";
import { MAX_DIFF_LINES, type DiffLine, type EditPreview } from "../diff";
import { useProjectRelative } from "../projectPath";
import { countEscapes, revealHidden, type RevealedPiece } from "../revealHidden";
import { HiddenWarning, Revealed } from "./Revealed";

/** What an `Edit`/`Write` call would do, shared by folded conversation rows (`maxLines`) and
 *  permission cards (the whole change, `maxLines` unset). `PreToolUse` carries the whole input.
 *
 *  Signal colours belong on the gutter, never on the text: `--nv-ok`/`--nv-error` are guarded at
 *  3:1 for non-text UI; text drawn in them measures 2.05-3.84:1 on rose-pine dawn. The code stays
 *  `--nv-fg`, enforced by `indexCss.test.ts`.
 *
 *  The file name and every diff line use `revealHidden`: a direction override can disguise the
 *  file being approved. `data-path` keeps the real path so opening it opens the file the call names.
 */
/** The most lines an expanded transcript row draws of one change. */
const ROW_MAX_LINES = 2 * MAX_DIFF_LINES;

export function EditDiff({
  preview,
  maxLines,
  createsFile,
  tool,
  whole = false,
}: {
  preview: EditPreview;
  maxLines?: number;
  createsFile?: boolean;
  /** The tool's name, shown before the path where the view is a transcript row's line (`✓ Edit path +5 −1`);
   *  left out inside a permission card, whose own head already names the tool. */
  tool?: string;
  /** Draw every line however many: a permission card approves exactly this change. A transcript row
   *  expanded past `ROW_MAX_LINES` shows the counts instead, since it is drawn again on every render. */
  whole?: boolean;
}) {
  const shownPath = useProjectRelative(preview.filePath);
  const full = !whole && preview.diff !== null && preview.diff.length > ROW_MAX_LINES ? null : preview.diff;
  const diff = full !== null && maxLines !== undefined && full.length > maxLines ? full.slice(0, maxLines) : full;
  const pathPieces = revealHidden(shownPath);
  const linePieces = diff?.map((line) => revealHidden(line.text)) ?? [];
  // The note can carry the call's own text (a `NotebookEdit` delete names its `cell_id`).
  const notePieces = preview.wholeFileNote === undefined ? null : revealHidden(preview.wholeFileNote);
  const hidden =
    countEscapes(pathPieces) +
    linePieces.reduce((sum, pieces) => sum + countEscapes(pieces), 0) +
    (notePieces === null ? 0 : countEscapes(notePieces));
  return (
    <div className="permission-card-edit">
      <div className="permission-card-edit-head">
        {tool !== undefined && <span className="permission-card-edit-tool">{tool}</span>}
        {/* The path first (after the tool's name, on a transcript row): which file is the question a reader asks before what changed. A real
            path is also a `gf`/click target; an empty one is not a path to open. */}
        {preview.filePath ? (
          <span className="permission-card-edit-path path-link" data-path={preview.filePath}>
            <Revealed pieces={pathPieces} />
          </span>
        ) : (
          <span className="permission-card-edit-path">(no file named)</span>
        )}
        <span className="permission-card-edit-counts">
          {/* The sign carries the colour, the number stays in the head's ink (`index.css`). */}
          <span className="edit-count-added">
            <span className="edit-sign">+</span>
            {preview.added}
          </span>{" "}
          <span className="edit-count-removed">
            <span className="edit-sign">−</span>
            {preview.removed}
          </span>
        </span>
      </div>
      <HiddenWarning count={hidden} what="change" />
      {preview.wholeFile &&
        (createsFile === true ? (
          /* Rust found nothing at the path when the card was raised, so nothing is
             overwritten -- a plain create is not a danger, and the warning below read as one. */
          <div className="permission-card-edit-note">Creates a new file.</div>
        ) : (
          /* Said rather than implied. A Write request carries only what the file WILL contain, so a
             patch-shaped rendering would suggest the rest of the file survives. It may not. Also
             what is shown when nobody looked (a snapshot from before `createsFile` existed, a call no card gated).
             `preview.wholeFileNote` overrides this wording for a tool whose whole
             content is not a whole file (`NotebookEdit`'s cell) -- `undefined` for `Write` keeps
             this exact pinned sentence. */
          <div className="permission-card-edit-note">
            {notePieces !== null ? <Revealed pieces={notePieces} /> : "Writes the whole file. The request does not say what is there now."}
          </div>
        ))}
      {preview.replaceAll && (
        /* The diff looks identical with and without this, so a reader cannot infer it. */
        <div className="permission-card-edit-note">
          Replaces <strong>every</strong> occurrence in the file, not just the first.
        </div>
      )}
      {diff === null ? (
        <div className="permission-card-edit-note">
          Too large to show here ({preview.added + preview.removed} lines). Shown as counts rather
          than as part of a diff, because a diff missing lines is one you would approve anyway.
        </div>
      ) : (
        <DiffRows lines={diff} pieces={linePieces} />
      )}
    </div>
  );
}

/** Shared text-only diff rows, including the mark for a missing final line break. */
export function DiffRows({ lines, pieces = lines.map((line) => revealHidden(line.text)) }: {
  lines: DiffLine[];
  pieces?: RevealedPiece[][];
}) {
  const starts = new Map(hunksOf(lines).map(({ start }, index) => [start, index]));
  return (
    <pre className="permission-card-diff">
      {lines.map((line, i) => (
        <div key={i} className={`diff-line diff-${line.kind}`} data-hunk={starts.get(i)}>
          <span className="diff-gutter" aria-hidden="true">
            {line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "}
          </span>
          <span className="diff-text">
            <Revealed pieces={pieces[i]} />
          </span>
        </div>
      )).flatMap((row, i) =>
        lines[i].noNewline === true
          ? [
              row,
              <div key={`${i}-noeol`} className="diff-line diff-noeol">
                <span className="diff-gutter" aria-hidden="true">
                  {" "}
                </span>
                <span className="diff-text">\ no line break at the end</span>
              </div>,
            ]
          : [row],
      )}
    </pre>
  );
}
