import type { PathRef } from "../paths";
import { revealedPathText } from "../revealHidden";
/** The compact footer list of `gf` paths without a shown target when the pick opens.
 *  Full HINT labels stay reachable here, with the typed prefix marked as on the in-place labels. */
export function PathPick({ refs, labels, typed }: { refs: PathRef[]; labels: string[]; typed: string }) {
  if (refs.length === 0) return null;
  return (
    <span className="path-pick" role="listbox">
      {refs.map((ref, i) => {
        const label = labels[i];
        const matches = label.startsWith(typed);
        return (
          <span key={i}>
            {i > 0 && " · "}
            <span className={matches ? undefined : "hint-off"}>
              {matches ? <><span className="hint-typed">{typed}</span>{label.slice(typed.length)}</> : label}
              {` ${revealedPathText(ref.path)}${ref.line === null ? "" : `:${ref.line}`}`}
            </span>
          </span>
        );
      })}
    </span>
  );
}
