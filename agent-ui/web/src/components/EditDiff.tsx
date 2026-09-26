import type { EditPreview } from "../diff";

/** What an `Edit`/`Write` call would do, rendered for a person to review.
 *
 * Moved out of `PermissionCard`'s `ToolInput` (Task 13, P3) so the same view can sit in a
 * conversation row (folded to `maxLines`) as well as in the card that gates the call (the whole
 * change, `maxLines` unset). The data has always been in the request -- `PreToolUse` carries the
 * whole tool-input object -- so this stays a rendering component with no wire of its own.
 *
 * **Signal colours are on the gutter, never on the text.** `--nv-ok`/`--nv-error` are guarded at
 * 3:1 for non-text UI, and drawing diff lines in them measured 2.05-3.84:1 on rose-pine dawn when
 * the same mistake was made in this file's banners. The `+`/`-` gutter carries the colour; the code
 * stays `--nv-fg`, and `indexCss.test.ts` enforces that.
 */
export function EditDiff({ preview, maxLines }: { preview: EditPreview; maxLines?: number }) {
  const diff = preview.diff !== null && maxLines !== undefined && preview.diff.length > maxLines ? preview.diff.slice(0, maxLines) : preview.diff;
  return (
    <div className="permission-card-edit">
      <div className="permission-card-edit-head">
        {/* The path first: which file is the question a reader asks before what changed. N2: a real
            path is also a `gf`/click target; an empty one is not a path to open. */}
        {preview.filePath ? (
          <span className="permission-card-edit-path path-link" data-path={preview.filePath}>
            {preview.filePath}
          </span>
        ) : (
          <span className="permission-card-edit-path">(no file named)</span>
        )}
        <span className="permission-card-edit-counts">
          +{preview.added} −{preview.removed}
        </span>
      </div>
      {preview.wholeFile && (
        /* Said rather than implied. A Write request carries only what the file WILL contain, so a
           patch-shaped rendering would suggest the rest of the file survives. It may not. */
        <div className="permission-card-edit-note">
          Writes the whole file. The request does not say what is there now.
        </div>
      )}
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
              <span className="diff-text">{line.text}</span>
            </div>
          ))}
        </pre>
      )}
    </div>
  );
}
