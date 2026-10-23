import type { EditPreview } from "../diff";
import { useProjectRelative } from "../projectPath";
import { countEscapes, revealHidden } from "../revealHidden";
import { HiddenWarning, Revealed } from "./Revealed";

/** What an `Edit`/`Write` call would do, rendered for a person to review.
 *
 * Moved out of `PermissionCard`'s `ToolInput` so the same view can sit in a
 * conversation row (folded to `maxLines`) as well as in the card that gates the call (the whole
 * change, `maxLines` unset). The data has always been in the request -- `PreToolUse` carries the
 * whole tool-input object -- so this stays a rendering component with no wire of its own.
 *
 * **Signal colours are on the gutter, never on the text.** `--nv-ok`/`--nv-error` are guarded at
 * 3:1 for non-text UI, and drawing diff lines in them measured 2.05-3.84:1 on rose-pine dawn when
 * the same mistake was made in this file's banners. The `+`/`-` gutter carries the colour; the code
 * stays `--nv-fg`, and `indexCss.test.ts` enforces that.
 *
 * The file name and every line of the change are drawn through `revealHidden`, as a `Bash` card's
 * command is: a card is raised for exactly the edits that need a human (outside the project, a
 * protected path), and a right-to-left override in a file name could make it read as another file.
 * `data-path` keeps the real path, so opening it opens the file the call names.
 */
export function EditDiff({
  preview,
  maxLines,
  createsFile,
  tool,
}: {
  preview: EditPreview;
  maxLines?: number;
  createsFile?: boolean;
  /** The tool's name, shown before the path where the view is a transcript row's line (`✓ Edit path +5 −1`);
   *  left out inside a permission card, whose own head already names the tool. */
  tool?: string;
}) {
  const shownPath = useProjectRelative(preview.filePath);
  const diff = preview.diff !== null && maxLines !== undefined && preview.diff.length > maxLines ? preview.diff.slice(0, maxLines) : preview.diff;
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
        <pre className="permission-card-diff">
          {diff.map((line, i) => (
            <div key={i} className={`diff-line diff-${line.kind}`}>
              <span className="diff-gutter" aria-hidden="true">
                {line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "}
              </span>
              <span className="diff-text">
                <Revealed pieces={linePieces[i]} />
              </span>
            </div>
          )).flatMap((row, i) =>
            diff[i].noNewline === true
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
      )}
    </div>
  );
}
